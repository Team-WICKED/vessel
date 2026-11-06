//! A per-address token bucket for new connections.
//!
//! IPv6 addresses count by their `/64`, the block a single host or home
//! usually gets, so one machine cannot dodge the limit by walking its
//! prefix. The table is bounded: idle buckets (full again) are dropped
//! when it grows, and when every bucket is busy a new address is let in
//! untracked rather than refused, so an attacker with many addresses
//! cannot lock everyone else out.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::time::Duration;

use tokio::time::Instant;

/// Addresses tracked at once.
const TRACKED: usize = 16_384;

#[derive(Debug)]
pub(crate) struct RateLimiter {
    burst: u32,
    refill: Duration,
    buckets: HashMap<IpAddr, Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: u32,
    updated: Instant,
}

impl RateLimiter {
    /// `burst` connections at once, then one more every `refill`.
    pub(crate) fn new(burst: u32, refill: Duration) -> Self {
        Self {
            burst,
            refill,
            buckets: HashMap::new(),
        }
    }

    /// Whether a new connection from `address` is admitted now.
    pub(crate) fn admit(&mut self, address: IpAddr, now: Instant) -> bool {
        let key = bucket_key(address);
        if !self.buckets.contains_key(&key) && self.buckets.len() >= TRACKED {
            self.prune(now);
            if self.buckets.len() >= TRACKED {
                return true;
            }
        }
        let (burst, refill) = (self.burst, self.refill);
        let bucket = self.buckets.entry(key).or_insert(Bucket {
            tokens: burst,
            updated: now,
        });
        let earned = earned(bucket, now, refill);
        if earned > 0 {
            bucket.tokens = bucket.tokens.saturating_add(earned).min(burst);
            bucket.updated = now;
        }
        match bucket.tokens.checked_sub(1) {
            Some(left) => {
                bucket.tokens = left;
                true
            }
            None => false,
        }
    }

    /// Drop buckets that would be full again by now.
    fn prune(&mut self, now: Instant) {
        let (burst, refill) = (self.burst, self.refill);
        self.buckets.retain(|_address, bucket| {
            bucket.tokens.saturating_add(earned(bucket, now, refill)) < burst
        });
    }
}

/// Tokens earned since the bucket was last updated.
fn earned(bucket: &Bucket, now: Instant, refill: Duration) -> u32 {
    let elapsed = now.saturating_duration_since(bucket.updated);
    let earned = elapsed
        .as_nanos()
        .checked_div(refill.as_nanos())
        .unwrap_or(u128::MAX);
    u32::try_from(earned).unwrap_or(u32::MAX)
}

/// IPv4 (and IPv4-mapped IPv6) by address, IPv6 by its `/64`.
pub(super) fn bucket_key(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(_) => address,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let [a, b, c, d, ..] = v6.segments();
            IpAddr::V6(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn a_burst_then_one_per_refill() {
        let mut limiter = RateLimiter::new(3, Duration::from_secs(1));
        let start = Instant::now();
        let address = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        assert!((0..3).all(|_| limiter.admit(address, start)));
        assert!(!limiter.admit(address, start));
        let later = start + Duration::from_millis(1500);
        assert!(limiter.admit(address, later));
        assert!(!limiter.admit(address, later));
        // Someone else is not affected.
        assert!(limiter.admit(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)), later));
    }

    #[test]
    fn ipv6_counts_by_its_64() {
        let mut limiter = RateLimiter::new(1, Duration::from_secs(60));
        let now = Instant::now();
        let first: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let same_block: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let other_block: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert!(limiter.admit(first, now));
        assert!(!limiter.admit(same_block, now));
        assert!(limiter.admit(other_block, now));
        let mapped: IpAddr = "::ffff:192.0.2.7".parse().unwrap();
        assert!(limiter.admit(mapped, now));
        assert!(!limiter.admit("192.0.2.7".parse().unwrap(), now));
    }

    #[test]
    fn the_table_stays_bounded() {
        let mut limiter = RateLimiter::new(1, Duration::from_secs(60));
        let now = Instant::now();
        for index in 0..u32::try_from(TRACKED).unwrap() {
            assert!(limiter.admit(IpAddr::V4(Ipv4Addr::from(index)), now));
        }
        assert_eq!(limiter.buckets.len(), TRACKED);
        // Every bucket is busy: a newcomer is let in, not tracked.
        assert!(limiter.admit(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)), now));
        assert_eq!(limiter.buckets.len(), TRACKED);
        // Once they are full again, they make room.
        let later = now + Duration::from_secs(61);
        assert!(limiter.admit(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 4)), later));
        assert_eq!(limiter.buckets.len(), 1);
    }
}
