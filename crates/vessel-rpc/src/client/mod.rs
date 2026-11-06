//! Client side: one connection to a node, shared by any number of callers.
//!
//! [`Connection::connect`] performs the handshake and spawns a driver task
//! that owns the transport (see `driver`). Callers talk to the driver
//! through a bounded command queue; answers come back on one-shot channels,
//! and each subscription gets its own bounded record channel.
//!
//! [Decision Log]
//! - Purpose: typed calls (`call::<R>()` returns `R::Response`) and
//!   subscriptions that never lose their first events and never block the
//!   connection.
//! - Plan deviation: `docs/plans/backend-m1.md` sketched `connect` returning
//!   one `Events` stream for all subscriptions. That shape has a race: the
//!   driver reads `Subscribed{S}` and then `Event{S}` back to back, and a
//!   router fed by a separate stream cannot know about `S` before the caller
//!   who received the reply registers it, so the first events are dropped.
//!   Here the driver itself creates a subscription's channel in the step that
//!   reads its reply, and [`Connection::subscribe`] returns that channel as a
//!   [`Subscription`].
//! - A slow subscriber never blocks the connection: when its channel is full
//!   the driver ends that subscription locally with `Lagged{resume_from}`
//!   (the same promise the node makes: nothing skipped, resume from there)
//!   and unsubscribes on the node. Blocking instead would stall every call
//!   on the connection behind it, including the `session.read` the
//!   subscriber needs to catch up: a deadlock.
//! - `session.subscribe` is not callable through [`Connection::call`]: its
//!   reply alone would leave the subscription's events with nowhere to go.
//!   The [`Unary`] marker makes that a compile error.

mod driver;
mod subscription;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vessel_proto::{
    Call, ClientFrame, ClientInfo, CreateSession, GetNode, GetSession, Hello, InviteDevice,
    KillSession, ListDevices, ListHarnesses, ListProfiles, ListSessions, MoveProfile, Outcome,
    ProtocolRange, ReadEvents, ReloadProfiles, Request, RequestId, RevokeDevice, SendInput,
    ServerFrame, StopNode, Subscribe, Unsubscribe, Welcome, WireError,
};

pub use subscription::{Subscription, SubscriptionItem};

use self::driver::{Command, Waiter};
use crate::error::{CallError, ConnectError, ProtocolViolation};
use crate::transport::FrameTransport;

/// A request answered by exactly one reply, callable through
/// [`Connection::call`]. Every method except `session.subscribe` (use
/// [`Connection::subscribe`]).
pub trait Unary: Request {}

impl Unary for GetNode {}
impl Unary for ListHarnesses {}
impl Unary for ListSessions {}
impl Unary for GetSession {}
impl Unary for CreateSession {}
impl Unary for SendInput {}
impl Unary for KillSession {}
impl Unary for ReadEvents {}
impl Unary for Unsubscribe {}
impl Unary for StopNode {}
impl Unary for InviteDevice {}
impl Unary for ListDevices {}
impl Unary for RevokeDevice {}
impl Unary for ListProfiles {}
impl Unary for ReloadProfiles {}
impl Unary for MoveProfile {}

/// `n` as a `NonZeroUsize` for constants.
const fn at_least_one(n: usize) -> NonZeroUsize {
    match NonZeroUsize::new(n) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    }
}

/// Limits for a client connection.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Commands (calls, cancels) queued for the driver; callers wait when
    /// it is full.
    pub command_capacity: NonZeroUsize,
    /// Records buffered per subscription. When a subscriber falls this far
    /// behind, its subscription ends with `Lagged` instead of blocking the
    /// connection.
    pub subscription_capacity: NonZeroUsize,
    /// How long to wait for the node's answer to `Hello`.
    pub handshake_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            command_capacity: at_least_one(64),
            subscription_capacity: at_least_one(1024),
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// A connection to a node. Cheap to clone; the connection closes when every
/// clone and every [`Subscription`] is dropped, or when the node leaves.
#[derive(Debug, Clone)]
pub struct Connection {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    commands: mpsc::Sender<Command>,
    ids: Arc<RequestIds>,
    welcome: Welcome,
    closed: CancellationToken,
}

/// Request ids for one connection, shared by callers and the driver.
#[derive(Debug, Default)]
pub(crate) struct RequestIds(AtomicU64);

impl RequestIds {
    /// The next unused id, or `None` once all 2^64 are used.
    pub(crate) fn next(&self) -> Option<RequestId> {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()
            .map(RequestId::new)
    }
}

/// The node's `Rejected` error, if that is what the handshake's answer is.
fn refusal<E>(
    answer: &Result<Option<Result<String, E>>, tokio::time::error::Elapsed>,
) -> Option<WireError> {
    let Ok(Some(Ok(text))) = answer else {
        return None;
    };
    match serde_json::from_str::<ServerFrame>(text) {
        Ok(ServerFrame::Rejected { error }) => Some(error),
        Ok(
            ServerFrame::Welcome(_)
            | ServerFrame::Response { .. }
            | ServerFrame::Event { .. }
            | ServerFrame::SubscriptionEnded { .. },
        )
        | Err(_) => None,
    }
}

impl Connection {
    /// Say hello over `transport` and start the connection's driver task.
    ///
    /// # Errors
    /// The transport failed, the node did not answer in time, refused the
    /// connection (for example no common protocol version), or broke the
    /// protocol.
    pub async fn connect<T: FrameTransport>(
        transport: T,
        client: ClientInfo,
        config: &ClientConfig,
    ) -> Result<Self, ConnectError<T::TransportError>> {
        let (mut sink, mut stream) = transport.split();
        let hello = ClientFrame::Hello(Hello {
            protocol: ProtocolRange::CURRENT,
            client,
        });
        let text = serde_json::to_string(&hello).map_err(ConnectError::Encode)?;
        let sent = sink.send(text).await;
        let answer = tokio::time::timeout(config.handshake_timeout, stream.next()).await;
        if let Err(error) = sent {
            // A node may refuse before reading `Hello` and close at once (a
            // wrong token on the local socket, ADR 0007), which breaks the
            // send. Its refusal can still be waiting to be read, and says
            // why; otherwise the send's own error does.
            return Err(
                refusal(&answer).map_or(ConnectError::Transport(error), ConnectError::Rejected)
            );
        }
        let answer = answer
            .map_err(|_elapsed| ConnectError::Timeout)?
            .ok_or(ConnectError::Closed)?
            .map_err(ConnectError::Transport)?;
        let welcome = match serde_json::from_str::<ServerFrame>(&answer) {
            Ok(ServerFrame::Welcome(welcome)) => welcome,
            Ok(ServerFrame::Rejected { error }) => return Err(ConnectError::Rejected(error)),
            Ok(
                ServerFrame::Response { .. }
                | ServerFrame::Event { .. }
                | ServerFrame::SubscriptionEnded { .. },
            ) => {
                return Err(ConnectError::Violation(ProtocolViolation::ExpectedWelcome));
            }
            Err(error) => return Err(ConnectError::Decode(error)),
        };

        let ids = Arc::new(RequestIds::default());
        let closed = CancellationToken::new();
        let (commands, commands_rx) = mpsc::channel(config.command_capacity.get());
        let driver = driver::Driver {
            commands: commands_rx,
            unsubscribe: commands.downgrade(),
            ids: Arc::clone(&ids),
            subscription_capacity: config.subscription_capacity,
            closed: closed.clone(),
        };
        drop(tokio::spawn(driver.run(sink, stream)));
        Ok(Self {
            shared: Arc::new(Shared {
                commands,
                ids,
                welcome,
                closed,
            }),
        })
    }

    /// The node's `Welcome`: negotiated protocol and node description.
    #[must_use]
    pub fn welcome(&self) -> &Welcome {
        &self.shared.welcome
    }

    /// Call a method and wait for its typed answer. Dropping the future
    /// cancels the call on the node.
    ///
    /// # Errors
    /// The node answered with an error, or the connection closed first.
    pub async fn call<R: Unary>(&self, request: R) -> Result<R::Response, CallError> {
        let (waiter, answer) = oneshot::channel();
        let pending = self
            .start(request.into_call(), Waiter::Unary(waiter))
            .await?;
        match pending.wait(answer).await? {
            Outcome::Ok(reply) => R::response_from(reply).map_err(CallError::UnexpectedReply),
            Outcome::Err(error) => Err(CallError::Remote(error)),
        }
    }

    /// Subscribe to a session's events. Dropping the returned
    /// [`Subscription`] unsubscribes.
    ///
    /// # Errors
    /// The node answered with an error, or the connection closed first.
    pub async fn subscribe(&self, request: Subscribe) -> Result<Subscription, CallError> {
        let (waiter, answer) = oneshot::channel();
        let pending = self
            .start(request.into_call(), Waiter::Subscribe(waiter))
            .await?;
        let opened = pending.wait(answer).await??;
        Ok(Subscription::new(opened, self.shared.commands.clone()))
    }

    /// Whether the connection has closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.is_cancelled()
    }

    /// Resolves once the connection has closed.
    pub async fn closed(&self) {
        self.shared.closed.cancelled().await;
    }

    async fn start(&self, call: Call, waiter: Waiter) -> Result<PendingCall<'_>, CallError> {
        let negotiated = self.shared.welcome.protocol;
        let needs = call.since();
        if needs > negotiated {
            return Err(CallError::NotInProtocol {
                method: call.method(),
                needs,
                negotiated,
            });
        }
        let id = self
            .shared
            .ids
            .next()
            .ok_or(CallError::RequestIdsExhausted)?;
        let command = Command::Call { id, call, waiter };
        // The queue is bounded; while waiting for room, also watch for the
        // connection closing, so a caller never waits on a dead driver.
        let queued = tokio::select! {
            biased;
            () = self.shared.closed.cancelled() => Err(CallError::Disconnected),
            queued = self.shared.commands.send(command) => {
                queued.map_err(|_closed| CallError::Disconnected)
            }
        };
        queued?;
        Ok(PendingCall {
            id,
            commands: &self.shared.commands,
            answered: false,
        })
    }
}

/// A call that was sent and not answered yet; cancels it on the node when
/// dropped unanswered.
struct PendingCall<'a> {
    id: RequestId,
    commands: &'a mpsc::Sender<Command>,
    answered: bool,
}

impl PendingCall<'_> {
    async fn wait<T>(mut self, answer: oneshot::Receiver<T>) -> Result<T, CallError> {
        let answer = answer.await;
        self.answered = true;
        answer.map_err(|_driver_gone| CallError::Disconnected)
    }
}

impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        if self.answered {
            return;
        }
        // Best effort: if the queue is full, the node finishes the call and
        // its answer is discarded when it arrives.
        if self.commands.try_send(Command::Cancel(self.id)).is_err() {
            tracing::trace!(id = %self.id, "could not send cancel");
        }
    }
}
