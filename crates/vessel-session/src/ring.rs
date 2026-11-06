//! [`Ring`]: recent records plus the live broadcast, read by followers.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;

use tokio::sync::broadcast;
use vessel_proto::{EventRecord, EventSeq};

/// The last `capacity` records of a session and the sender of its live
/// stream.
///
/// Written only by the session actor, read by [`follow`](Ring::follow).
/// Both happen under one lock, so a follower's snapshot and its broadcast
/// subscription are taken at the same point in the stream: every record is
/// either in the snapshot or on the receiver, never both and never neither.
/// Records are `Arc`s shared with the durable channel and every follower,
/// so fan-out costs a reference count per reader, not a copy.
#[derive(Debug)]
pub(crate) struct Ring {
    records: VecDeque<Arc<EventRecord>>,
    capacity: NonZeroUsize,
    /// Sequence number of the next record (the end of the log).
    next: EventSeq,
    /// `None` once the session has ended, so live receivers see `Closed`
    /// right after the final record instead of waiting forever.
    live: Option<broadcast::Sender<Arc<EventRecord>>>,
}

impl Ring {
    /// An empty ring.
    pub(crate) fn new(capacity: NonZeroUsize) -> Self {
        let (live, _) = broadcast::channel(capacity.get());
        Self {
            records: VecDeque::with_capacity(capacity.get()),
            capacity,
            next: EventSeq::ZERO,
            live: Some(live),
        }
    }

    /// Append the next record (seqs are consecutive) and broadcast it.
    pub(crate) fn append(&mut self, record: Arc<EventRecord>) {
        if self.records.len() >= self.capacity.get() {
            drop(self.records.pop_front());
        }
        // The publisher never hands out the last `u64`, so `next` exists.
        self.next = record.seq.next().unwrap_or(record.seq);
        if let Some(live) = &self.live {
            // Fails only when nobody follows live right now; the ring and
            // the store still have the record.
            drop(live.send(Arc::clone(&record)));
        }
        self.records.push_back(record);
    }

    /// End the live stream: current and future receivers see `Closed` after
    /// the records already sent.
    pub(crate) fn close(&mut self) {
        self.live = None;
    }

    /// Whether the session can publish no more records (its actor dropped
    /// the publisher, after the final status).
    pub(crate) const fn is_closed(&self) -> bool {
        self.live.is_none()
    }

    /// Cached records from `from`, plus a receiver for everything after.
    /// After the session ended the receiver is already closed.
    pub(crate) fn follow(&self, from: EventSeq) -> Result<Follow, FollowError> {
        if from > self.next {
            return Err(FollowError::Ahead { next: self.next });
        }
        let oldest = self.records.front().map_or(self.next, |record| record.seq);
        let Some(skip) = from.get().checked_sub(oldest.get()) else {
            return Err(FollowError::Gap {
                oldest_cached: oldest,
            });
        };
        // `oldest <= from <= next` and the ring holds `oldest..next`, so the
        // skip is within the ring and fits a `usize`.
        let skip = usize::try_from(skip).unwrap_or(usize::MAX);
        let cached = self.records.iter().skip(skip).cloned().collect();
        let live = match &self.live {
            Some(live) => live.subscribe(),
            None => broadcast::channel(1).1,
        };
        Ok(Follow { cached, live })
    }
}

/// A follower's starting point: cached records, then the live stream.
#[derive(Debug)]
pub struct Follow {
    /// Records from the requested `from` up to the moment of subscribing,
    /// in order.
    pub cached: Vec<Arc<EventRecord>>,
    /// Every record after `cached`, in order, then `RecvError::Closed` once
    /// the session's final status has been sent. Bounded by the ring
    /// capacity: a receiver that falls further behind gets
    /// `RecvError::Lagged`, and the follower resumes from the store.
    pub live: broadcast::Receiver<Arc<EventRecord>>,
}

/// Why [`SessionHandle::follow`](crate::SessionHandle::follow) cannot start
/// at the requested record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FollowError {
    /// `from` is older than anything in the ring. The node reads
    /// `from..oldest_cached` from the store and follows again from
    /// `oldest_cached`.
    #[error("records before {oldest_cached} are no longer cached")]
    Gap {
        /// The oldest record still in the ring.
        oldest_cached: EventSeq,
    },
    /// `from` is past the end of the log; nothing with that number exists
    /// yet.
    #[error("the log ends at {next}")]
    Ahead {
        /// Sequence number of the next record (the end of the log).
        next: EventSeq,
    },
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use vessel_proto::{SessionEvent, SessionStatus};

    use super::*;

    fn ring(capacity: usize) -> Ring {
        Ring::new(NonZeroUsize::new(capacity).unwrap())
    }

    fn record(seq: u64) -> Arc<EventRecord> {
        Arc::new(EventRecord {
            seq: EventSeq::new(seq),
            at: DateTime::UNIX_EPOCH,
            event: SessionEvent::Status {
                status: SessionStatus::Running,
            },
        })
    }

    fn seqs(records: &[Arc<EventRecord>]) -> Vec<u64> {
        records.iter().map(|record| record.seq.get()).collect()
    }

    #[test]
    fn keeps_the_newest_records() {
        let mut ring = ring(3);
        for seq in 0..5 {
            ring.append(record(seq));
        }
        assert_eq!(
            seqs(&ring.follow(EventSeq::new(2)).unwrap().cached),
            [2, 3, 4]
        );
        assert_eq!(seqs(&ring.follow(EventSeq::new(4)).unwrap().cached), [4]);
        assert!(ring.follow(EventSeq::new(5)).unwrap().cached.is_empty());
        assert_eq!(
            ring.follow(EventSeq::new(1)).unwrap_err(),
            FollowError::Gap {
                oldest_cached: EventSeq::new(2)
            }
        );
    }

    #[test]
    fn snapshot_and_live_stream_are_disjoint() {
        let mut ring = ring(8);
        ring.append(record(0));
        let mut follow = ring.follow(EventSeq::ZERO).unwrap();
        ring.append(record(1));
        assert_eq!(seqs(&follow.cached), [0]);
        assert_eq!(follow.live.try_recv().unwrap().seq, EventSeq::new(1));
        assert!(follow.live.try_recv().is_err());
    }

    #[test]
    fn closing_ends_current_and_future_followers() {
        let mut ring = ring(4);
        ring.append(record(0));
        let mut before = ring.follow(EventSeq::ZERO).unwrap();
        ring.append(record(1));
        ring.close();
        assert_eq!(before.live.try_recv().unwrap().seq, EventSeq::new(1));
        assert_eq!(
            before.live.try_recv().unwrap_err(),
            broadcast::error::TryRecvError::Closed
        );
        let mut after = ring.follow(EventSeq::ZERO).unwrap();
        assert_eq!(seqs(&after.cached), [0, 1]);
        assert_eq!(
            after.live.try_recv().unwrap_err(),
            broadcast::error::TryRecvError::Closed
        );
    }

    #[test]
    fn following_past_the_end_is_an_error() {
        let mut ring = ring(2);
        assert!(ring.follow(EventSeq::ZERO).unwrap().cached.is_empty());
        assert_eq!(
            ring.follow(EventSeq::new(1)).unwrap_err(),
            FollowError::Ahead {
                next: EventSeq::ZERO
            }
        );
        ring.append(record(0));
        assert!(ring.follow(EventSeq::new(1)).unwrap().cached.is_empty());
        assert_eq!(
            ring.follow(EventSeq::new(u64::MAX)).unwrap_err(),
            FollowError::Ahead {
                next: EventSeq::new(1)
            }
        );
    }

    #[test]
    fn followers_share_records_instead_of_copying_them() {
        let mut ring = ring(2);
        let first = record(0);
        ring.append(Arc::clone(&first));
        let follow = ring.follow(EventSeq::ZERO).unwrap();
        assert!(Arc::ptr_eq(&follow.cached[0], &first));
    }
}
