//! [`ResumingStream`]: a session stream that outlives its connection.
//!
//! [Decision Log]
//! - Purpose: over SSH (ADR 0007) a connection drops whenever the network
//!   does (a laptop sleeps, Wi-Fi changes). An attached terminal should say
//!   so and carry on, not end the attach.
//! - Constraint: the node keeps no per-connection state worth restoring,
//!   and every record has a durable sequence number (ADR 0006). So
//!   resuming is a read-only problem: dial again, follow from the cursor.
//!   A call that was in flight when the connection dropped may or may not
//!   have been applied, and nothing here can know which.
//! - Alternatives: Zed's replay buffer with acknowledgements (needed when
//!   the state lives in the stream; ours lives in the node, and the buffer
//!   is unbounded); retrying calls after a reconnect (a `session.send` or a
//!   `session.kill` could be applied twice).
//! - Choice: this stream redials through a [`Connector`] with capped,
//!   jittered exponential backoff ([`Backoff`]), follows again from its
//!   cursor with an ordinary [`SessionStream`] (which already makes the
//!   records gap-free and unrepeated), and reports the loss and the resume
//!   as non-fatal items ([`StreamError::Reconnecting`],
//!   [`StreamError::Resumed`]). It never sends anything that changes state:
//!   callers send input through the connection it publishes
//!   ([`ResumingStream::client`]), and a send that failed with
//!   `Disconnected` stays failed.
//! - Cancellation: `next` is used inside `select!` loops that drop it
//!   whenever another branch wins (a key press). The dial in progress is
//!   kept in the stream's state and the backoff wait is a deadline, so
//!   dropping `next` loses nothing and a slow SSH login is not restarted on
//!   every key.
//! - Costs: one boxed future per dial; a connection that drops right after
//!   every successful resume redials at the shortest delay each time (the
//!   backoff restarts once a resume succeeds), which a full dial and
//!   subscribe bound to a few attempts per second.

use std::fmt;
use std::future::Future;
use std::hash::{BuildHasher as _, RandomState};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use vessel_proto::{ClientInfo, EventRecord, EventSeq, SessionId};
use vessel_rpc::CallError;

use crate::client::NodeClient;
use crate::connector::Connector;
use crate::error::StreamError;
use crate::stream::SessionStream;

/// How long to wait between reconnect attempts: doubling from `first` up
/// to `max`, each delay scaled by a random factor between 0.75 and 1.25
/// and kept within `first..=max`, so many clients that lost the same node
/// do not all come back in the same instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The delay before the first attempt.
    pub first: Duration,
    /// The longest delay.
    pub max: Duration,
}

impl Default for Backoff {
    /// 0.5 s to 30 s (milestone 2 plan, C1). Thirty seconds keeps a resumed
    /// terminal within half a minute of the network coming back.
    fn default() -> Self {
        Self {
            first: Duration::from_millis(500),
            max: Duration::from_secs(30),
        }
    }
}

impl Backoff {
    /// The delay before attempt `attempt` (from 1).
    #[must_use]
    pub fn delay(&self, attempt: u32) -> Duration {
        let doubling = 1_u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        let base = self.first.saturating_mul(doubling).min(self.max);
        // 750..1250 per mille. `RandomState` is seeded randomly per process
        // and changes with every instance: enough for jitter, no new
        // dependency.
        let per_mille = RandomState::new()
            .hash_one(attempt)
            .checked_rem(500)
            .and_then(|spread| u32::try_from(spread).ok())
            .unwrap_or(0)
            .saturating_add(750);
        base.saturating_mul(per_mille)
            .checked_div(1000)
            .unwrap_or(base)
            .clamp(self.first, self.max.max(self.first))
    }
}

/// Follow `session` from `from` through `connector`, reconnecting whenever
/// the connection drops; see [`ResumingStream`]. Nothing is dialled until
/// the first [`next`](ResumingStream::next) (or use
/// [`ResumingStream::reusing`] to start on a connection already open).
#[must_use]
pub fn follow_resuming<C: Connector + 'static>(
    connector: C,
    client: ClientInfo,
    session: SessionId,
    from: EventSeq,
) -> ResumingStream<C> {
    ResumingStream {
        connector: Arc::new(connector),
        client_info: client,
        session,
        cursor: from,
        backoff: Backoff::default(),
        state: State::Dial { attempt: 0 },
        current: watch::Sender::new(None),
    }
}

/// A session's records in order, like [`SessionStream`], across lost
/// connections: when the connection drops it yields
/// [`StreamError::Reconnecting`], dials again with backoff, follows again
/// from its cursor, and yields [`StreamError::Resumed`]. Both are
/// non-fatal; the records around them are gap-free and unrepeated.
///
/// It ends (`None`) when the session ends, or after a fatal error: the
/// session is gone, or a dial failed in a way that waiting does not fix
/// ([`Connector::is_transient`]). It does not give up on a node that stays
/// down; drop it to stop.
///
/// [`next`](Self::next) is cancel-safe.
pub struct ResumingStream<C> {
    connector: Arc<C>,
    client_info: ClientInfo,
    session: SessionId,
    /// The next sequence number to yield.
    cursor: EventSeq,
    backoff: Backoff,
    state: State,
    /// The open connection, `None` while there is none.
    current: watch::Sender<Option<NodeClient>>,
}

impl<C> fmt::Debug for ResumingStream<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumingStream")
            .field("session", &self.session)
            .field("cursor", &self.cursor)
            .field("backoff", &self.backoff)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// Where the stream is.
enum State {
    /// Dial now. `attempt` 0 is the first connection, which is not a
    /// reconnect.
    Dial { attempt: u32 },
    /// A dial in progress.
    Dialing { attempt: u32, dial: Dial },
    /// Waiting before attempt `attempt`.
    Waiting { attempt: u32, until: Instant },
    /// Following on an open connection. (Boxed: a stream is several
    /// hundred bytes, the other states a few words.)
    Following(Box<SessionStream>),
    /// The session ended, or a fatal error was reported.
    Done,
}

impl fmt::Debug for State {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dial { attempt } => write!(formatter, "Dial({attempt})"),
            Self::Dialing { attempt, .. } => write!(formatter, "Dialing({attempt})"),
            Self::Waiting { attempt, until } => {
                write!(formatter, "Waiting({attempt}, {until:?})")
            }
            Self::Following(stream) => write!(formatter, "Following({:?})", stream.cursor()),
            Self::Done => formatter.write_str("Done"),
        }
    }
}

/// A dial: connect, then follow from the cursor.
type Dial = Pin<Box<dyn Future<Output = Dialed> + Send>>;

/// How a dial ended.
enum Dialed {
    Following(NodeClient, Box<SessionStream>),
    /// Failed; `transient` if trying again may help.
    Failed {
        transient: bool,
        reason: String,
    },
    /// Connected, but the session cannot be followed (it is gone).
    Refused(CallError),
}

impl<C: Connector + 'static> ResumingStream<C> {
    /// Start on `client`, a connection already open to the same node,
    /// instead of dialling one. Later connections are dialled. `client` is
    /// published ([`client`](Self::client)) at once: input can be sent on
    /// it before the first record is read.
    #[must_use]
    pub fn reusing(mut self, client: NodeClient) -> Self {
        let session = self.session;
        let from = self.cursor;
        let _none = self.current.send_replace(Some(client.clone()));
        let dial: Dial = Box::pin(async move { follow(client, session, from).await });
        self.state = State::Dialing { attempt: 0, dial };
        self
    }

    /// Use `backoff` between reconnect attempts.
    #[must_use]
    pub const fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// The sequence number of the next record this stream will yield.
    #[must_use]
    pub const fn cursor(&self) -> EventSeq {
        self.cursor
    }

    /// The open connection, for sending input: `None` while reconnecting.
    /// Each new connection replaces the value. A call made on a connection
    /// that then drops fails with `Disconnected` (it may or may not have
    /// been applied); it is not repeated on the next one.
    #[must_use]
    pub fn client(&self) -> watch::Receiver<Option<NodeClient>> {
        self.current.subscribe()
    }

    /// The next record, or a non-fatal notice about the connection; `None`
    /// once the session has ended or after a fatal error.
    pub async fn next(&mut self) -> Option<Result<EventRecord, StreamError>> {
        loop {
            match &mut self.state {
                State::Done => return None,
                State::Dial { attempt } => {
                    let attempt = *attempt;
                    let dial = self.dial();
                    self.state = State::Dialing { attempt, dial };
                }
                State::Waiting { attempt, until } => {
                    let attempt = *attempt;
                    tokio::time::sleep_until(*until).await;
                    self.state = State::Dial { attempt };
                }
                State::Dialing { attempt, dial } => {
                    let attempt = *attempt;
                    match dial.await {
                        Dialed::Following(client, stream) => {
                            self.state = State::Following(stream);
                            let _previous = self.current.send_replace(Some(client));
                            if attempt > 0 {
                                tracing::debug!(session = %self.session, at = %self.cursor, "resumed");
                                return Some(Err(StreamError::Resumed { at: self.cursor }));
                            }
                        }
                        Dialed::Failed {
                            transient: true,
                            reason,
                        } => return Some(Err(self.retry(attempt.saturating_add(1), reason))),
                        Dialed::Failed {
                            transient: false,
                            reason,
                        } => {
                            self.state = State::Done;
                            return Some(Err(StreamError::Unreachable { reason }));
                        }
                        Dialed::Refused(error) => {
                            self.state = State::Done;
                            return Some(Err(StreamError::Call(error)));
                        }
                    }
                }
                State::Following(stream) => {
                    let item = stream.next().await;
                    self.cursor = stream.cursor();
                    match item {
                        None => {
                            self.state = State::Done;
                            return None;
                        }
                        Some(Err(StreamError::Call(CallError::Disconnected))) => {
                            let _lost = self.current.send_replace(None);
                            tracing::debug!(session = %self.session, at = %self.cursor, "connection lost");
                            return Some(Err(self.retry(1, "the connection closed".to_owned())));
                        }
                        Some(item) => {
                            if item.as_ref().is_err_and(StreamError::is_fatal) {
                                self.state = State::Done;
                            }
                            return Some(item);
                        }
                    }
                }
            }
        }
    }

    /// Wait before attempt `attempt`, and say so.
    fn retry(&mut self, attempt: u32, reason: String) -> StreamError {
        let retry_in = self.backoff.delay(attempt);
        let until = Instant::now()
            .checked_add(retry_in)
            .unwrap_or_else(Instant::now);
        self.state = State::Waiting { attempt, until };
        StreamError::Reconnecting {
            attempt,
            retry_in,
            reason,
        }
    }

    fn dial(&self) -> Dial {
        let connector = Arc::clone(&self.connector);
        let client_info = self.client_info.clone();
        let session = self.session;
        let from = self.cursor;
        Box::pin(async move {
            match connector.connect(client_info).await {
                Ok(client) => follow(client, session, from).await,
                Err(error) => Dialed::Failed {
                    transient: connector.is_transient(&error),
                    reason: chain(&error),
                },
            }
        })
    }
}

async fn follow(client: NodeClient, session: SessionId, from: EventSeq) -> Dialed {
    match client.follow(session, from).await {
        Ok(stream) => Dialed::Following(client, Box::new(stream)),
        Err(CallError::Disconnected) => Dialed::Failed {
            transient: true,
            reason: "the connection closed while subscribing".to_owned(),
        },
        Err(error) => Dialed::Refused(error),
    }
}

/// `error` and its causes, `: `-separated, for one line of text.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_up_to_the_cap_with_jitter() {
        let backoff = Backoff::default();
        for _ in 0..50 {
            let first = backoff.delay(1);
            assert!(first >= Duration::from_millis(500), "{first:?}");
            assert!(first <= Duration::from_millis(625), "{first:?}");
            let third = backoff.delay(3);
            assert!(third >= Duration::from_millis(1500), "{third:?}");
            assert!(third < Duration::from_millis(2500), "{third:?}");
            for attempt in [7, 20, 64, u32::MAX] {
                let late = backoff.delay(attempt);
                assert!(late >= Duration::from_millis(22_500), "{late:?}");
                assert!(late <= Duration::from_secs(30), "{late:?}");
            }
        }
        // The factor actually varies.
        let delays: std::collections::HashSet<_> = (0..50).map(|_| backoff.delay(5)).collect();
        assert!(delays.len() > 1);
    }

    #[test]
    fn a_backoff_whose_cap_is_below_its_start_waits_its_start() {
        let backoff = Backoff {
            first: Duration::from_secs(2),
            max: Duration::from_secs(1),
        };
        assert_eq!(backoff.delay(4), Duration::from_secs(2));
    }
}
