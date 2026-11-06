//! A per-address token bucket for new connections.
//!
//! An address counts in every block it belongs to (`address::blocks`: an
//! IPv4 address; an IPv6 `/64`, `/56` and `/48`), each with a bucket of its
//! own, the wider blocks with a burst and a refill scaled up as their
//! limits are (`Block::scale`). A connection is admitted only when every
//! one of its blocks has a token, and then takes one from each: neither one
//! host nor one `/48` walking its prefixes gets more than its share. The
//! table is bounded: idle buckets (full again) are dropped when it grows,
//! and a block that cannot be tracked is let through untracked rather than
//! refused, so an attacker with many addresses cannot lock everyone else
//! out.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use tokio::time::Instant;

use super::address::{Block, blocks};

/// Blocks tracked at once.
const TRACKED: usize = 16_384;

#[derive(Debug)]
pub(crate) struct RateLimiter {
    burst: u32,
    refill: Duration,
    buckets: HashMap<Block, Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: u32,
    updated: Instant,
}

impl RateLimiter {
    /// `burst` connections at once, then one more every `refill`, per
    /// address (scaled for wider blocks).
    pub(crate) fn new(burst: u32, refill: Duration) -> Self {
        Self {
            burst,
            refill,
            buckets: HashMap::new(),
        }
    }

    /// Whether a new connection from `address` is admitted now.
    pub(crate) fn admit(&mut self, address: IpAddr, now: Instant) -> bool {
        let blocks = blocks(address);
        let mut tracked = Vec::with_capacity(blocks.len());
        for block in blocks {
            if self.track(block, now) {
                tracked.push(block);
            }
        }
        let has_token = |limiter: &Self, block: &Block| {
            limiter
                .buckets
                .get(block)
                .is_none_or(|bucket| bucket.tokens > 0)
        };
        if !tracked.iter().all(|block| has_token(self, block)) {
            return false;
        }
        for block in &tracked {
            if let Some(bucket) = self.buckets.get_mut(block) {
                bucket.tokens = bucket.tokens.saturating_sub(1);
            }
        }
        true
    }

    /// Give back the token a connection from `address` took: it
    /// authenticated, and only connections that never do spend the rate.
    /// Never past a block's burst; nothing for a block no longer tracked.
    pub(crate) fn refund(&mut self, address: IpAddr) {
        for block in blocks(address) {
            let (burst, _refill) = self.scaled(block);
            if let Some(bucket) = self.buckets.get_mut(&block) {
                bucket.tokens = bucket.tokens.saturating_add(1).min(burst);
            }
        }
    }

    /// Bring `block`'s bucket up to `now`, making one if there is room;
    /// whether it is tracked.
    fn track(&mut self, block: Block, now: Instant) -> bool {
        if !self.buckets.contains_key(&block) && self.buckets.len() >= TRACKED {
            self.prune(now);
            if self.buckets.len() >= TRACKED {
                return false;
            }
        }
        let (burst, refill) = self.scaled(block);
        let bucket = self.buckets.entry(block).or_insert(Bucket {
            tokens: burst,
            updated: now,
        });
        let earned = earned(bucket, now, refill);
        if earned > 0 {
            bucket.tokens = bucket.tokens.saturating_add(earned).min(burst);
            bucket.updated = now;
        }
        true
    }

    /// `block`'s burst and refill.
    fn scaled(&self, block: Block) -> (u32, Duration) {
        let scale = block.scale();
        (
            self.burst.saturating_mul(scale),
            self.refill.checked_div(scale).unwrap_or(self.refill),
        )
    }

    /// Drop buckets that would be full again by now.
    fn prune(&mut self, now: Instant) {
        let (burst, refill) = (self.burst, self.refill);
        self.buckets.retain(|block, bucket| {
            let scale = block.scale();
            let refill = refill.checked_div(scale).unwrap_or(refill);
            bucket.tokens.saturating_add(earned(bucket, now, refill)) < burst.saturating_mul(scale)
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
    fn a_refund_gives_a_token_back_up_to_the_burst() {
        let mut limiter = RateLimiter::new(2, Duration::from_secs(3600));
        let now = Instant::now();
        let address: IpAddr = "2001:db8:9:1::1".parse().unwrap();
        assert!(limiter.admit(address, now));
        assert!(limiter.admit(address, now));
        assert!(!limiter.admit(address, now));
        limiter.refund(address);
        assert!(limiter.admit(address, now));
        assert!(!limiter.admit(address, now));
        // Refunds past the burst are not kept.
        for _ in 0..5 {
            limiter.refund(address);
        }
        assert!(limiter.admit(address, now));
        assert!(limiter.admit(address, now));
        assert!(!limiter.admit(address, now));
    }

    #[test]
    fn ipv6_counts_by_its_64_and_a_48_by_four_times_that() {
        let mut limiter = RateLimiter::new(1, Duration::from_secs(60));
        let now = Instant::now();
        let first: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let same_block: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        assert!(limiter.admit(first, now));
        assert!(!limiter.admit(same_block, now));
        // Other /64s of the same /48: four in all, then the /48 is spent.
        for other in ["2001:db8:1:3::1", "2001:db8:1:104::1", "2001:db8:1:205::1"] {
            assert!(limiter.admit(other.parse().unwrap(), now), "{other}");
        }
        assert!(!limiter.admit("2001:db8:1:306::1".parse().unwrap(), now));
        // A /56 is spent at two: its third /64 is refused.
        let mut limiter = RateLimiter::new(1, Duration::from_secs(60));
        assert!(limiter.admit("2001:db8:2:1::1".parse().unwrap(), now));
        assert!(limiter.admit("2001:db8:2:2::1".parse().unwrap(), now));
        assert!(!limiter.admit("2001:db8:2:3::1".parse().unwrap(), now));
        // Another /48 is not affected; a mapped address is its IPv4 one.
        assert!(limiter.admit("2001:db8:3:1::1".parse().unwrap(), now));
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
