//! The startup check that one IPv6 `/48` cannot hold half of a shared pool,
//! whatever the operator sets ([`check`], `Limits::check`).
//!
//! [Decision Log] (after the security review's round 8)
//! - Purpose: the bar is that a single host, or a single `/48`, must not
//!   be able to deny the relay. At the defaults it holds; but every limit
//!   is a flag, and a `/48` may hold four times each per-address limit
//!   (`address::WIDEST`), so `--max-registering 16` with the default
//!   per-address 7 would let one `/48` take every registering place.
//! - Choice: refuse to start when, for any pool, what one `/48` may hold
//!   is half of the pool or more: registering places, client places, and
//!   sockets not authenticated yet; and when the unauthenticated share is
//!   half of `max_connections` or more, so the share kept for sockets
//!   that authenticated is always the larger. The refusal names both
//!   flags, the largest value of the first that would pass and the
//!   smallest pool that would.
//! - Alternatives: clamping the per-address limits to fit (a relay quietly
//!   running with limits nobody set); a warning (the bar would then hold
//!   only for operators who read logs).
//! - Cost: small relays need small per-address limits (a pool of 16 takes
//!   a per-address limit of 1). Tests that exercise one cap at a tiny
//!   scale turn the check off (`RelayConfig::check_limits`); `vesseld
//!   relay` cannot.

use std::fmt;

use super::Limits;
use super::address::WIDEST;

/// One pool and the share of it one holder may take.
struct Rule {
    /// The flag of the share, without `--`.
    share: &'static str,
    value: usize,
    /// How many times `value` the holder may take.
    scale: u32,
    /// Who holds it, for the refusal.
    holder: Holder,
    /// The flag of the pool, without `--`.
    pool: &'static str,
    total: usize,
}

impl Rule {
    /// What the holder may take of the pool.
    fn held(&self) -> usize {
        self.value.saturating_mul(scale(self.scale))
    }

    /// Whether the holder takes less than half of the pool.
    fn holds(&self) -> bool {
        self.held().saturating_mul(2) < self.total
    }

    fn refusal(&self) -> Unbalanced {
        let per_value = scale(self.scale).saturating_mul(2);
        Unbalanced {
            share: self.share,
            value: self.value,
            holder: self.holder,
            held: self.held(),
            pool: self.pool,
            total: self.total,
            largest: self
                .total
                .saturating_sub(1)
                .checked_div(per_value)
                .unwrap_or(0),
            least: self.held().saturating_mul(2).saturating_add(1),
        }
    }
}

fn scale(scale: u32) -> usize {
    usize::try_from(scale).unwrap_or(usize::MAX)
}

/// Who a rule keeps under half of a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    /// One IPv6 `/48`, at [`WIDEST`] times the per-address limit.
    One48,
    /// The connections not authenticated yet, together.
    Unauthenticated,
}

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::One48 => write!(f, "one IPv6 /48 ({WIDEST} times the per-address limit)"),
            Self::Unauthenticated => f.write_str("connections not authenticated yet"),
        }
    }
}

/// The rules, in the order they are checked.
fn rules(limits: &Limits) -> [Rule; 4] {
    [
        Rule {
            share: "max-registering-per-address",
            value: limits.max_registering_per_address,
            scale: WIDEST,
            holder: Holder::One48,
            pool: "max-registering",
            total: limits.max_registering,
        },
        Rule {
            share: "max-clients-per-address",
            value: limits.max_clients_per_address,
            scale: WIDEST,
            holder: Holder::One48,
            pool: "max-clients",
            total: limits.max_clients,
        },
        Rule {
            share: "max-connections-per-address",
            value: limits.max_connections_per_address,
            scale: WIDEST,
            holder: Holder::One48,
            pool: "max-unauthenticated",
            total: limits.max_unauthenticated,
        },
        Rule {
            share: "max-unauthenticated",
            value: limits.max_unauthenticated,
            scale: 1,
            holder: Holder::Unauthenticated,
            pool: "max-connections",
            total: limits.max_connections,
        },
    ]
}

/// The first rule `limits` breaks, if any.
pub(super) fn check(limits: &Limits) -> Result<(), Unbalanced> {
    match rules(limits).iter().find(|rule| !rule.holds()) {
        Some(rule) => Err(rule.refusal()),
        None => Ok(()),
    }
}

/// Limits under which one IPv6 `/48` could hold half of a pool or more, or
/// the connections not authenticated yet half of all (`Limits::check`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct Unbalanced {
    share: &'static str,
    value: usize,
    holder: Holder,
    held: usize,
    pool: &'static str,
    total: usize,
    /// The largest `share` that would pass with this pool (0: none does).
    largest: usize,
    /// The smallest pool that would pass with this `share`.
    least: usize,
}

impl fmt::Display for Unbalanced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "--{} {} lets {} hold {}, half or more of --{} {}: ",
            self.share, self.value, self.holder, self.held, self.pool, self.total
        )?;
        if self.largest == 0 {
            write!(f, "no --{} passes", self.share)?;
        } else {
            write!(f, "--{} {} at most passes", self.share, self.largest)?;
        }
        write!(f, ", or --{} {} at least", self.pool, self.least)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_pass() {
        assert_eq!(check(&Limits::default()), Ok(()));
    }

    #[test]
    fn one_48_holds_less_than_half_of_the_registering_places() {
        let mut limits = Limits {
            max_registering: 64,
            max_registering_per_address: 7,
            ..Limits::default()
        };
        assert_eq!(check(&limits), Ok(()));
        limits.max_registering_per_address = 8;
        let refusal = check(&limits).unwrap_err();
        assert_eq!(
            refusal.to_string(),
            "--max-registering-per-address 8 lets one IPv6 /48 (4 times the per-address \
             limit) hold 32, half or more of --max-registering 64: \
             --max-registering-per-address 7 at most passes, or --max-registering 65 at least"
        );
        // A pool too small for any per-address limit says so.
        limits.max_registering = 8;
        limits.max_registering_per_address = 1;
        let refusal = check(&limits).unwrap_err().to_string();
        assert!(
            refusal.contains("no --max-registering-per-address passes"),
            "{refusal}"
        );
        assert!(
            refusal.contains("--max-registering 9 at least"),
            "{refusal}"
        );
    }

    #[test]
    fn one_48_holds_less_than_half_of_the_client_places() {
        let mut limits = Limits {
            max_clients: 129,
            max_clients_per_address: 16,
            ..Limits::default()
        };
        assert_eq!(check(&limits), Ok(()));
        limits.max_clients = 128;
        let refusal = check(&limits).unwrap_err().to_string();
        assert!(
            refusal.starts_with("--max-clients-per-address 16 ")
                && refusal.contains("half or more of --max-clients 128")
                && refusal.contains("--max-clients-per-address 15 at most"),
            "{refusal}"
        );
    }

    #[test]
    fn one_48_holds_less_than_half_of_the_unauthenticated_share() {
        let mut limits = Limits {
            max_unauthenticated: 200,
            max_connections_per_address: 24,
            ..Limits::default()
        };
        assert_eq!(check(&limits), Ok(()));
        limits.max_connections_per_address = 25;
        let refusal = check(&limits).unwrap_err().to_string();
        assert!(
            refusal.starts_with("--max-connections-per-address 25 ")
                && refusal.contains("half or more of --max-unauthenticated 200")
                && refusal.contains("--max-connections-per-address 24 at most"),
            "{refusal}"
        );
    }

    #[test]
    fn the_unauthenticated_share_is_less_than_half_of_all() {
        let mut limits = Limits {
            max_connections: 1000,
            max_unauthenticated: 499,
            ..Limits::default()
        };
        assert_eq!(check(&limits), Ok(()));
        limits.max_unauthenticated = 500;
        let refusal = check(&limits).unwrap_err().to_string();
        assert!(
            refusal.starts_with("--max-unauthenticated 500 lets connections not authenticated")
                && refusal.contains("half or more of --max-connections 1000")
                && refusal.contains("--max-unauthenticated 499 at most")
                && refusal.contains("--max-connections 1001 at least"),
            "{refusal}"
        );
    }
}
