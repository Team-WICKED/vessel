//! The client connection's driver task: a writer and a reader side by side.
//!
//! - The writer takes commands from callers, registers each call's waiter
//!   with the reader, then writes the frame.
//! - The reader owns the pending calls and the subscription routes. It never
//!   waits on anyone but the transport: answers go to one-shot channels and
//!   records go to bounded per-subscription channels with `try_send`, so a
//!   caller who stopped listening cannot stall the connection.
//!
//! A waiter is registered before its request is written, so its answer can
//! never arrive first; the reader also drains pending registrations before
//! handling any answer, which keeps that true even though the two run as
//! separate futures.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::Arc;

use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vessel_proto::{
    Call, ClientFrame, EventRecord, Method, Outcome, Reply, Request as _, RequestId, ServerFrame,
    Subscribed, SubscriptionEnd, SubscriptionId, UnexpectedReply, Unsubscribe,
};

use super::RequestIds;
use crate::error::{CallError, ProtocolViolation};

/// Registrations in flight from writer to reader. The reader drains them
/// before every answer, so this only needs to absorb a burst.
const REGISTRATION_CAPACITY: usize = 64;

/// What callers ask of the driver.
#[derive(Debug)]
pub(super) enum Command {
    /// Send a call; answer `waiter`.
    Call {
        id: RequestId,
        call: Call,
        waiter: Waiter,
    },
    /// The caller stopped waiting for this call.
    Cancel(RequestId),
    /// Fire-and-forget `session.unsubscribe` (a subscription was dropped or
    /// fell behind). Its answer is ignored.
    Unsubscribe(SubscriptionId),
}

/// Who is waiting for a call's answer.
#[derive(Debug)]
pub(super) enum Waiter {
    /// An ordinary call.
    Unary(oneshot::Sender<Outcome>),
    /// `session.subscribe`: the reader opens a route when the reply arrives.
    Subscribe(oneshot::Sender<Result<Opened, CallError>>),
}

/// A subscription the reader has opened a route for.
#[derive(Debug)]
pub(super) struct Opened {
    pub(super) subscribed: Subscribed,
    pub(super) records: mpsc::Receiver<EventRecord>,
    pub(super) end: oneshot::Receiver<SubscriptionEnd>,
}

/// Writer to reader.
enum Registration {
    Wait(RequestId, Waiter),
    Forget(RequestId),
}

/// Why the driver stopped; only logged, since callers learn of it through
/// their own channels closing.
#[derive(Debug, thiserror::Error)]
enum DriverError<E> {
    #[error("transport failed")]
    Transport(#[source] E),
    #[error("undecodable frame from the node")]
    Decode(#[source] serde_json::Error),
    #[error("could not encode a frame")]
    Encode(#[source] serde_json::Error),
    #[error("protocol violation")]
    Violation(#[source] ProtocolViolation),
}

/// Everything the driver task owns besides the transport.
pub(super) struct Driver {
    pub(super) commands: mpsc::Receiver<Command>,
    /// For the reader's own unsubscribes. Weak, so the driver does not keep
    /// the connection alive after every handle is gone.
    pub(super) unsubscribe: mpsc::WeakSender<Command>,
    pub(super) ids: Arc<RequestIds>,
    pub(super) subscription_capacity: NonZeroUsize,
    pub(super) closed: CancellationToken,
}

impl Driver {
    /// Run until the node leaves, the transport fails, or every handle is
    /// dropped. Dropping the reader's state on the way out fails every
    /// pending call and ends every subscription with `Disconnected`.
    pub(super) async fn run<K, S, E>(self, sink: K, stream: S)
    where
        K: Sink<String, Error = E> + Unpin,
        S: Stream<Item = Result<String, E>> + Unpin,
        E: std::error::Error,
    {
        let (register, registrations) = mpsc::channel(REGISTRATION_CAPACITY);
        let writer = write(sink, self.commands, register, self.ids, self.closed.clone());
        let reader = Reader {
            registrations,
            pending: HashMap::new(),
            routes: HashMap::new(),
            draining: HashSet::new(),
            unsubscribe: self.unsubscribe,
            capacity: self.subscription_capacity,
        };
        let (written, read) = tokio::join!(writer, reader.run(stream, self.closed));
        for result in [written, read] {
            if let Err(error) = result {
                tracing::debug!(error = %error, "connection to node ended");
            }
        }
    }
}

async fn write<K, E>(
    mut sink: K,
    mut commands: mpsc::Receiver<Command>,
    register: mpsc::Sender<Registration>,
    ids: Arc<RequestIds>,
    closed: CancellationToken,
) -> Result<(), DriverError<E>>
where
    K: Sink<String, Error = E> + Unpin,
{
    let _close_on_exit = closed.clone().drop_guard();
    loop {
        let command = tokio::select! {
            biased;
            () = closed.cancelled() => break,
            command = commands.recv() => command,
        };
        // `None`: every `Connection` and `Subscription` is gone.
        let Some(command) = command else { break };
        let frame = match command {
            Command::Call { id, call, waiter } => {
                if register.send(Registration::Wait(id, waiter)).await.is_err() {
                    break;
                }
                ClientFrame::Request { id, call }
            }
            Command::Cancel(id) => {
                if register.send(Registration::Forget(id)).await.is_err() {
                    break;
                }
                ClientFrame::Cancel { id }
            }
            Command::Unsubscribe(subscription) => {
                let Some(id) = ids.next() else { continue };
                ClientFrame::Request {
                    id,
                    call: Unsubscribe { subscription }.into_call(),
                }
            }
        };
        let text = serde_json::to_string(&frame).map_err(DriverError::Encode)?;
        sink.feed(text).await.map_err(DriverError::Transport)?;
        if commands.is_empty() {
            sink.flush().await.map_err(DriverError::Transport)?;
        }
    }
    if sink.close().await.is_err() {
        tracing::trace!("node gone while closing");
    }
    Ok(())
}

/// Where one subscription's records go.
struct Route {
    records: mpsc::Sender<EventRecord>,
    end: oneshot::Sender<SubscriptionEnd>,
}

impl Route {
    /// Deliver the end reason. Dropping `records` lets the subscriber drain
    /// what was buffered before it sees the end.
    fn finish(self, subscription: SubscriptionId, end: SubscriptionEnd) {
        if self.end.send(end).is_err() {
            tracing::trace!(%subscription, "subscriber gone before its end");
        }
    }
}

struct Reader {
    registrations: mpsc::Receiver<Registration>,
    pending: HashMap<RequestId, Waiter>,
    routes: HashMap<SubscriptionId, Route>,
    /// Subscriptions we asked the node to end and whose
    /// `SubscriptionEnded` has not arrived; their frames are dropped.
    draining: HashSet<SubscriptionId>,
    unsubscribe: mpsc::WeakSender<Command>,
    capacity: NonZeroUsize,
}

impl Reader {
    async fn run<S, E>(
        mut self,
        mut stream: S,
        closed: CancellationToken,
    ) -> Result<(), DriverError<E>>
    where
        S: Stream<Item = Result<String, E>> + Unpin,
    {
        let _close_on_exit = closed.clone().drop_guard();
        loop {
            tokio::select! {
                biased;
                () = closed.cancelled() => return Ok(()),
                Some(registration) = self.registrations.recv() => self.register(registration),
                frame = stream.next() => {
                    let Some(frame) = frame else { return Ok(()) };
                    let text = frame.map_err(DriverError::Transport)?;
                    let frame = serde_json::from_str::<ServerFrame>(&text)
                        .map_err(DriverError::Decode)?;
                    self.handle(frame).map_err(DriverError::Violation)?;
                }
            }
        }
    }

    fn register(&mut self, registration: Registration) {
        match registration {
            Registration::Wait(id, waiter) => {
                if self.pending.insert(id, waiter).is_some() {
                    tracing::error!(%id, "request id registered twice");
                }
            }
            Registration::Forget(id) => drop(self.pending.remove(&id)),
        }
    }

    fn handle(&mut self, frame: ServerFrame) -> Result<(), ProtocolViolation> {
        match frame {
            ServerFrame::Response { id, outcome } => {
                while let Ok(registration) = self.registrations.try_recv() {
                    self.register(registration);
                }
                self.answer(id, outcome);
            }
            ServerFrame::Event {
                subscription,
                record,
            } => self.deliver(subscription, record),
            ServerFrame::SubscriptionEnded { subscription, end } => {
                if let Some(route) = self.routes.remove(&subscription) {
                    route.finish(subscription, end);
                }
                let _was_draining = self.draining.remove(&subscription);
            }
            ServerFrame::Welcome(_) | ServerFrame::Rejected { .. } => {
                return Err(ProtocolViolation::UnexpectedHandshake);
            }
        }
        Ok(())
    }

    fn answer(&mut self, id: RequestId, outcome: Outcome) {
        match self.pending.remove(&id) {
            Some(Waiter::Unary(waiter)) => {
                if waiter.send(outcome).is_err() {
                    tracing::trace!(%id, "caller stopped waiting");
                }
            }
            Some(Waiter::Subscribe(waiter)) => self.open(waiter, outcome),
            // A cancelled call answered anyway. If it opened a subscription,
            // nobody will read it: close it on the node.
            None => {
                if let Outcome::Ok(Reply::Subscribe(subscribed)) = outcome {
                    self.unsubscribe(subscribed.subscription);
                }
            }
        }
    }

    fn open(&mut self, waiter: oneshot::Sender<Result<Opened, CallError>>, outcome: Outcome) {
        let opened = match outcome {
            Outcome::Ok(Reply::Subscribe(subscribed)) => {
                let (records_tx, records) = mpsc::channel(self.capacity.get());
                let (end_tx, end) = oneshot::channel();
                let route = Route {
                    records: records_tx,
                    end: end_tx,
                };
                if self.routes.insert(subscribed.subscription, route).is_some() {
                    tracing::warn!(subscription = %subscribed.subscription, "node reused a subscription id");
                }
                Ok(Opened {
                    subscribed,
                    records,
                    end,
                })
            }
            Outcome::Ok(reply) => Err(CallError::UnexpectedReply(UnexpectedReply {
                expected: Method::Subscribe,
                got: reply.method(),
            })),
            Outcome::Err(error) => Err(CallError::Remote(error)),
        };
        if let Err(Ok(unclaimed)) = waiter.send(opened) {
            let subscription = unclaimed.subscribed.subscription;
            drop(self.routes.remove(&subscription));
            self.unsubscribe(subscription);
        }
    }

    fn deliver(&mut self, subscription: SubscriptionId, record: EventRecord) {
        let Some(route) = self.routes.get(&subscription) else {
            if !self.draining.contains(&subscription) {
                // A stray subscription (its unsubscribe could not be queued
                // earlier): try again.
                self.unsubscribe(subscription);
            }
            return;
        };
        match route.records.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(record)) => {
                // The subscriber fell behind. End it here the way the node
                // would: nothing skipped, resume from the first record it
                // did not get.
                if let Some(route) = self.routes.remove(&subscription) {
                    let resume_from = record.seq;
                    tracing::debug!(%subscription, %resume_from, "subscriber lagged");
                    route.finish(subscription, SubscriptionEnd::Lagged { resume_from });
                }
                self.unsubscribe(subscription);
            }
            Err(TrySendError::Closed(_)) => {
                drop(self.routes.remove(&subscription));
                self.unsubscribe(subscription);
            }
        }
    }

    /// Ask the node to end `subscription`, without waiting. If the command
    /// queue is full the request is skipped; the next stray frame for the
    /// subscription tries again.
    fn unsubscribe(&mut self, subscription: SubscriptionId) {
        let queued = self.unsubscribe.upgrade().is_some_and(|commands| {
            commands
                .try_send(Command::Unsubscribe(subscription))
                .is_ok()
        });
        if queued {
            let _newly_draining = self.draining.insert(subscription);
        }
    }
}
