//! [`Ring`]: recent records plus the live broadcast, read by followers.

use std::collections::VecDeque;

use tokio::sync::broadcast;
use vessel_proto::{EventRecord, EventSeq};

/// The last `capacity` records of a session and the sender of its live
/// stream.
///
/// Written only by the session actor, read by [`follow`](Ring::follow).
/// Both happen under one lock, so a follower's snapshot and its broadcast
/// subscription are taken at the same point in the stream: every record is
/// either in the snapshot or on the receiver, never both and never neither.
#[derive(Debug)]
pub(crate) struct Ring {
    records: VecDeque<EventRecord>,
    capacity: usize,
    /// `None` once the session has ended, so live receivers see `Closed`
    /// right after the final record instead of waiting forever.
    live: Option<broadcast::Sender<EventRecord>>,
}

impl Ring {
    /// An empty ring; `capacity` must be at least 1.
    pub(crate) fn new(capacity: usize) -> Self {
        let (live, _) = broadcast::channel(capacity);
        Self {
            records: VecDeque::with_capacity(capacity),
            capacity,
            live: Some(live),
        }
    }

    /// Append the next record (seqs are consecutive) and broadcast it.
    pub(crate) fn append(&mut self, record: EventRecord) {
        if self.records.len() >= self.capacity {
            drop(self.records.pop_front());
        }
        self.records.push_back(record.clone());
        if let Some(live) = &self.live {
            // Fails only when nobody follows live right now; the ring and
            // the store still have the record.
            drop(live.send(record));
        }
    }

    /// End the live stream: current and future receivers see `Closed` after
    /// the records already sent.
    pub(crate) fn close(&mut self) {
        self.live = None;
    }

    /// Cached records from `from`, plus a receiver for everything after.
    ///
    /// `from` beyond the end of the log yields no cached records, and the
    /// receiver starts at the current end; callers ask for at most
    /// `next_seq`. After the session ended the receiver is already closed.
    pub(crate) fn follow(&self, from: EventSeq) -> Result<Follow, Gap> {
        let oldest = self
            .records
            .front()
            .map_or(EventSeq::ZERO, |record| record.seq);
        let Some(skip) = from.get().checked_sub(oldest.get()) else {
            return Err(Gap {
                oldest_cached: oldest,
            });
        };
        let cached = match usize::try_from(skip) {
            Ok(skip) => self.records.iter().skip(skip).cloned().collect(),
            // Further ahead than any ring can hold: nothing cached.
            Err(_) => Vec::new(),
        };
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
    pub cached: Vec<EventRecord>,
    /// Every record after `cached`, in order, then `RecvError::Closed` once
    /// the session's final status has been sent. Bounded by the ring
    /// capacity: a receiver that falls further behind gets
    /// `RecvError::Lagged`, and the follower resumes from the store.
    pub live: broadcast::Receiver<EventRecord>,
}

/// `from` is older than anything in the ring.
///
/// The node reads `from..oldest_cached` from the store and calls
/// [`SessionHandle::follow`](crate::SessionHandle::follow) again with
/// `oldest_cached`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("records before {oldest_cached} are no longer cached")]
pub struct Gap {
    /// The oldest record still in the ring.
    pub oldest_cached: EventSeq,
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use vessel_proto::{SessionEvent, SessionStatus};

    use super::*;

    fn record(seq: u64) -> EventRecord {
        EventRecord {
            seq: EventSeq::new(seq),
            at: DateTime::UNIX_EPOCH,
            event: SessionEvent::Status {
                status: SessionStatus::Running,
            },
        }
    }

    fn seqs(records: &[EventRecord]) -> Vec<u64> {
        records.iter().map(|record| record.seq.get()).collect()
    }

    #[test]
    fn keeps_the_newest_records() {
        let mut ring = Ring::new(3);
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
            Gap {
                oldest_cached: EventSeq::new(2)
            }
        );
    }

    #[test]
    fn snapshot_and_live_stream_are_disjoint() {
        let mut ring = Ring::new(8);
        ring.append(record(0));
        let mut follow = ring.follow(EventSeq::ZERO).unwrap();
        ring.append(record(1));
        assert_eq!(seqs(&follow.cached), [0]);
        assert_eq!(follow.live.try_recv().unwrap().seq, EventSeq::new(1));
        assert!(follow.live.try_recv().is_err());
    }

    #[test]
    fn closing_ends_current_and_future_followers() {
        let mut ring = Ring::new(4);
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
    fn far_future_requests_get_no_cache() {
        let mut ring = Ring::new(2);
        ring.append(record(0));
        assert!(
            ring.follow(EventSeq::new(u64::MAX))
                .unwrap()
                .cached
                .is_empty()
        );
    }
}
