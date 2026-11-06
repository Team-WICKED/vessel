//! `session.subscribe`: forwards one session's records to one subscription.
//!
//! A forwarder keeps a cursor (the next sequence number to send) and gets
//! records from wherever they are:
//!
//! - the session's in-memory ring and live broadcast (`follow`), when the
//!   cursor is still in the ring;
//! - the store, for anything older. It first waits until the store has
//!   committed everything up to the ring's oldest record, because a record
//!   can leave the ring before the store has it;
//! - the store alone, for ended sessions the store serves (from earlier
//!   runs, or handed over once their log was complete).
//!
//! A forwarder never buffers. When the client is slow, sending waits on the
//! connection's outbound queue; meanwhile the live broadcast moves on, and
//! once it has overwritten records this forwarder has not sent, the
//! subscription ends with `Lagged{resume_from: cursor}`. The client then
//! catches up at its own pace with `session.read` (ADR 0006).

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use tokio_util::task::TaskTracker;
use vessel_proto::{EventRecord, EventSeq, SessionId, Subscribe, Subscribed, SubscriptionEnd};
use vessel_rpc::{Outbound, SubscriptionSink};
use vessel_session::{Follow, FollowError, SessionHandle};
use vessel_store::Store;

use crate::error::ServiceError;
use crate::persist::{self, Durability};
use crate::sessions::{Found, Sessions};

/// Records read from the store per page while filling a gap.
const PAGE: u32 = 256;

/// Where a new subscription starts reading.
enum Source {
    Live {
        handle: SessionHandle,
        durability: watch::Receiver<Durability>,
        /// The first `follow`, taken while handling the call so the
        /// subscription's view starts no later than the call.
        first: Result<Follow, FollowError>,
    },
    Stored,
}

/// The subscription was closed by the client or the connection.
struct Closed;

/// Open a subscription and start its forwarder. Everything that can fail
/// is checked before the subscription is opened, so a failed call leaves
/// nothing behind.
pub(crate) async fn open(
    sessions: &Sessions,
    outbound: &Outbound,
    forwarders: &TaskTracker,
    request: Subscribe,
) -> Result<Subscribed, ServiceError> {
    let Subscribe { session, from } = request;
    let ahead = |next| ServiceError::Ahead {
        session,
        from,
        next,
    };
    let (info, source) = match sessions.find(session).await? {
        Found::Live { handle, durability } => {
            let first = handle.follow(from);
            if let Err(FollowError::Ahead { next }) = first {
                return Err(ahead(next));
            }
            (
                handle.info(),
                Source::Live {
                    handle,
                    durability,
                    first,
                },
            )
        }
        Found::Stored(info) => {
            if from > info.next_seq {
                return Err(ahead(info.next_seq));
            }
            (info, Source::Stored)
        }
    };
    let sink = outbound.open_subscription(from).await?;
    let subscription = sink.id();
    let forwarder = Forwarder {
        store: sessions.store().clone(),
        session,
        cursor: from,
        sink,
    };
    // No `.await` between spawning and returning: if this call is
    // cancelled, it is cancelled before the forwarder exists.
    drop(forwarders.spawn(forwarder.run(source)));
    Ok(Subscribed { subscription, info })
}

struct Forwarder {
    store: Store,
    session: SessionId,
    /// The next sequence number to send.
    cursor: EventSeq,
    sink: SubscriptionSink,
}

impl Forwarder {
    async fn run(mut self, source: Source) {
        let ended = match source {
            Source::Live {
                handle,
                durability,
                first,
            } => self.live(&handle, durability, first).await,
            Source::Stored => self.stored().await,
        };
        match ended {
            Ok(end) => self.sink.end(end).await,
            Err(Closed) => {
                tracing::trace!(session = %self.session, "subscription closed by the client");
            }
        }
    }

    async fn live(
        &mut self,
        handle: &SessionHandle,
        mut durability: watch::Receiver<Durability>,
        first: Result<Follow, FollowError>,
    ) -> Result<SubscriptionEnd, Closed> {
        let mut attempt = first;
        loop {
            match attempt {
                Ok(follow) => return self.stream(follow).await,
                Err(FollowError::Gap { oldest_cached }) => {
                    if !self.replay(&mut durability, oldest_cached).await? {
                        return Ok(self.lagged());
                    }
                }
                // The cursor only moves over records that exist, so the log
                // cannot be behind it; resubscribing sorts out whatever
                // happened.
                Err(FollowError::Ahead { .. }) => return Ok(self.lagged()),
            }
            attempt = handle.follow(self.cursor);
        }
    }

    /// Send what the ring holds, then live records, until the session ends
    /// or this subscription falls behind the broadcast.
    async fn stream(&mut self, follow: Follow) -> Result<SubscriptionEnd, Closed> {
        let Follow { cached, mut live } = follow;
        for record in cached {
            self.send(record).await?;
        }
        loop {
            let received = tokio::select! {
                biased;
                () = self.sink.cancelled() => return Err(Closed),
                received = live.recv() => received,
            };
            match received {
                Ok(record) => self.send(record).await?,
                Err(RecvError::Lagged(missed)) => {
                    tracing::debug!(session = %self.session, missed, "subscriber lagged");
                    return Ok(self.lagged());
                }
                // The session ended and every record was delivered.
                Err(RecvError::Closed) => return Ok(SubscriptionEnd::SessionEnded),
            }
        }
    }

    /// Send stored records from the cursor up to `until` (exclusive), once
    /// the store has them. Returns `false` if the store cannot provide them.
    async fn replay(
        &mut self,
        durability: &mut watch::Receiver<Durability>,
        until: EventSeq,
    ) -> Result<bool, Closed> {
        let waited = tokio::select! {
            biased;
            () = self.sink.cancelled() => return Err(Closed),
            waited = persist::wait_until(self.session, durability, until) => waited,
        };
        if let Err(error) = waited {
            tracing::warn!(session = %self.session, %error, "cannot fill a subscription gap");
            return Ok(false);
        }
        while self.cursor < until {
            let Some(page) = self.page().await else {
                return Ok(false);
            };
            let mut sent_any = false;
            for record in page.into_iter().take_while(|record| record.seq < until) {
                self.send(Arc::new(record)).await?;
                sent_any = true;
            }
            if !sent_any {
                // Retention pruned everything from the cursor up to `until`.
                // Move the cursor to `until` so the next `follow` starts at
                // the ring; the client sees the jump as missing records.
                // Keeping the cursor would ask the ring for the same gap
                // again, forever (a busy loop re-reading the store).
                self.cursor = self.cursor.max(until);
                break;
            }
        }
        Ok(true)
    }

    /// A session the store serves: everything stored, then the end.
    async fn stored(&mut self) -> Result<SubscriptionEnd, Closed> {
        loop {
            let Some(records) = self.page().await else {
                return Ok(self.lagged());
            };
            if records.is_empty() {
                return Ok(SubscriptionEnd::SessionEnded);
            }
            for record in records {
                self.send(Arc::new(record)).await?;
            }
        }
    }

    /// The next page of stored records from the cursor; `None` if the store
    /// failed.
    async fn page(&self) -> Option<Vec<EventRecord>> {
        match self
            .store
            .read_events(self.session, self.cursor, PAGE)
            .await
        {
            Ok(page) => Some(page.records),
            Err(error) => {
                tracing::warn!(session = %self.session, %error, "cannot read the session log");
                None
            }
        }
    }

    /// Send one record and advance the cursor past it. Records before the
    /// cursor were already sent and are skipped. A record after the cursor
    /// means older ones were pruned; it is sent, and the client sees the
    /// jump.
    async fn send(&mut self, record: Arc<EventRecord>) -> Result<(), Closed> {
        if record.seq < self.cursor {
            return Ok(());
        }
        let next = record.seq.next();
        self.sink.send(record).await.map_err(|_closed| Closed)?;
        // `None` only after u64::MAX records: the cursor stays, and the
        // next record (there is none) would be skipped.
        if let Some(next) = next {
            self.cursor = next;
        }
        Ok(())
    }

    /// End now; the client resumes from the cursor with `session.read`.
    const fn lagged(&self) -> SubscriptionEnd {
        SubscriptionEnd::Lagged {
            resume_from: self.cursor,
        }
    }
}
