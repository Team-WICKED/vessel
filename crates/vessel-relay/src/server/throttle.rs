//! Two small pieces of the relay's limits: the byte rate of one direction
//! of a relayed connection ([`ByteRate`]), and logging refusals without
//! letting a flood of them flood the log ([`RefusalLog`]).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// One direction of a relayed connection, held to `rate` bytes a second
/// after a burst of one second's worth.
///
/// [Decision Log]
/// - Purpose: one connection must not take a relay's whole bandwidth
///   (ADR 0009 keeps relays dumb and cheap to run; the security review's
///   round 6 found no limit at all).
/// - Choice: the generic cell rate algorithm: each message moves a
///   "theoretical arrival time" on by its cost (`len / rate`), and the
///   message waits until that time minus the burst allowance. One
///   instant of state, exact for any message size, no timer of its own.
///   The relay waits before forwarding and reads nothing meanwhile, so
///   TCP pushes back on the sender and nothing is buffered here.
/// - Alternatives: a token bucket refilled by a timer (a task per
///   connection); counting bytes per window (bursts at window edges).
/// - Limits: the rate is at least one largest message a second, so a
///   message never waits more than about a second and the connection's
///   pings, sent from the same loop, keep their rhythm.
#[derive(Debug)]
pub(crate) struct ByteRate {
    rate: u64,
    tolerance: Duration,
    /// When the bytes passed so far would have finished at exactly `rate`.
    arrival: Instant,
}

impl ByteRate {
    /// `rate` bytes a second, at least `min` (one largest message); `None`
    /// for no limit (`rate` 0).
    pub(crate) fn new(rate: u64, min: u64, now: Instant) -> Option<Self> {
        if rate == 0 {
            return None;
        }
        let rate = rate.max(min).max(1);
        Some(Self {
            rate,
            tolerance: cost(rate, rate),
            arrival: now,
        })
    }

    /// How long `len` bytes arriving `now` wait before they may go on; they
    /// count as sent once that wait is over.
    pub(crate) fn take(&mut self, len: u64, now: Instant) -> Duration {
        let start = self.arrival.max(now);
        self.arrival = start.checked_add(cost(len, self.rate)).unwrap_or(start);
        let allowed = self.arrival.checked_sub(self.tolerance).unwrap_or(now);
        allowed.saturating_duration_since(now)
    }
}

/// How long `len` bytes take at `rate` bytes a second.
fn cost(len: u64, rate: u64) -> Duration {
    let nanos = u128::from(len)
        .saturating_mul(1_000_000_000)
        .checked_div(u128::from(rate))
        .unwrap_or(0);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// What a relay refused, for its log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Refusal {
    /// A registration past `Limits::max_nodes`.
    Nodes,
    /// A control connection past `Limits::max_registering`.
    Registering,
    /// A client past `Limits::max_clients`.
    Clients,
    /// A client past `Limits::max_clients_per_node`.
    NodeClients,
    /// A control connection past `Limits::max_registering_per_address`.
    AddressRegistering,
    /// A node not registered before, in a place kept for those that were
    /// (`Limits::reserved_registering`).
    NotKnown,
    /// A client past `Limits::max_clients_per_address`.
    AddressClients,
    /// A socket past `Limits::max_connections`, at `accept`.
    Connections,
    /// A socket past `Limits::max_connections_per_address`, at `accept`.
    AddressConnections,
    /// A socket past `Limits::max_unauthenticated`, at `accept`.
    Unauthenticated,
    /// A new connection past its address's rate (`Limits::connect_burst`).
    AcceptRate,
    /// A client past `Limits::max_node_clients_per_address`.
    NodeAddressClients,
    /// A trusted proxy's client address header that is not one address.
    AddressHeader,
}

/// How often each kind of refusal is logged at most.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// Refusals to log: the first of a kind at once, then at most one line per
/// kind every [`LOG_EVERY`], counting the ones in between, so someone
/// knocking thousands of times costs a few lines.
#[derive(Debug, Default)]
pub(crate) struct RefusalLog {
    kinds: Mutex<HashMap<Refusal, Noted>>,
}

#[derive(Debug)]
struct Noted {
    logged: Instant,
    since: u64,
}

impl RefusalLog {
    /// Note a refusal `now`: `Some(count)` when it is to be logged, with
    /// how many of its kind were refused since the last line.
    pub(crate) fn note(&self, refusal: Refusal, now: Instant) -> Option<u64> {
        let mut kinds = self
            .kinds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match kinds.get_mut(&refusal) {
            Some(noted) if now.saturating_duration_since(noted.logged) < LOG_EVERY => {
                noted.since = noted.since.saturating_add(1);
                None
            }
            Some(noted) => {
                let skipped = noted.since;
                *noted = Noted {
                    logged: now,
                    since: 0,
                };
                Some(skipped)
            }
            None => {
                let _first = kinds.insert(
                    refusal,
                    Noted {
                        logged: now,
                        since: 0,
                    },
                );
                Some(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;

    #[test]
    fn a_burst_passes_then_bytes_wait_their_share_of_a_second() {
        let start = Instant::now();
        let mut rate = ByteRate::new(64 * KIB, 64 * KIB, start).unwrap();
        // One second's worth passes at once.
        assert_eq!(rate.take(32 * KIB, start), Duration::ZERO);
        assert_eq!(rate.take(32 * KIB, start), Duration::ZERO);
        // The next 16 KiB waits a quarter of a second, the next half.
        assert_eq!(rate.take(16 * KIB, start), Duration::from_millis(250));
        assert_eq!(rate.take(16 * KIB, start), Duration::from_millis(500));
        // A pause earns the allowance back, never more than one second's.
        let later = start + Duration::from_secs(10);
        assert_eq!(rate.take(64 * KIB, later), Duration::ZERO);
        assert_eq!(rate.take(64 * KIB, later), Duration::from_secs(1));
    }

    #[test]
    fn no_rate_is_no_limit_and_a_tiny_one_is_raised_to_a_message_a_second() {
        let now = Instant::now();
        assert!(ByteRate::new(0, 64 * KIB, now).is_none());
        let mut slow = ByteRate::new(1, 64 * KIB, now).unwrap();
        assert_eq!(slow.take(64 * KIB, now), Duration::ZERO);
        assert_eq!(slow.take(64 * KIB, now), Duration::from_secs(1));
    }

    #[test]
    fn refusals_are_logged_once_per_kind_and_counted_in_between() {
        let log = RefusalLog::default();
        let start = Instant::now();
        assert_eq!(log.note(Refusal::Clients, start), Some(0));
        assert_eq!(log.note(Refusal::Clients, start), None);
        assert_eq!(log.note(Refusal::Clients, start), None);
        // Another kind has its own line.
        assert_eq!(log.note(Refusal::Nodes, start), Some(0));
        let later = start + LOG_EVERY;
        assert_eq!(log.note(Refusal::Clients, later), Some(2));
        assert_eq!(log.note(Refusal::Clients, later), None);
    }
}
