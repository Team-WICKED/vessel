//! Where a peer counts for the per-address limits ([`blocks`],
//! [`BlockCounts`]), and which peers may say where a client really is
//! ([`TrustedProxy`]). Public because a node counts its direct route's
//! unadmitted channels the same way (`vessel-node`'s secure gateway, the
//! review's round 10): one layering, one set of multipliers.
//!
//! [Decision Log] (the security review, rounds 7 and 8)
//! - Purpose: one host, or one IPv6 `/48` (what a single customer is
//!   usually given), must not be able to deny a relay. Counting IPv6 by
//!   its `/64` alone let a `/56` fill every limit by walking its prefixes.
//! - Choice: an IPv4 address counts as itself; an IPv6 address counts in
//!   three nested blocks, its `/64`, `/56` and `/48`, and every per-address
//!   limit holds in each: the `/64` gets the limit as configured, the `/56`
//!   twice it, the `/48` four times it ([`Block::scale`]). An IPv4-mapped
//!   IPv6 address counts as the IPv4 address it maps.
//! - Alternatives: a limit per `/48` only (one home's devices, each with
//!   its own `/64`, would share one small limit); a configurable limit per
//!   layer (fifteen knobs for one idea).
//! - Behind a reverse proxy the client's address comes from a header, but
//!   only from a peer the operator named (`--trusted-proxy`): a header from
//!   anyone else would let them pick their address and slip every limit.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// How many times a per-address limit one IPv6 `/48` may hold: the most
/// any block may. The counting ([`V6_LAYERS`]) and the startup check that
/// no `/48` can hold half of a pool (`Limits::check`) both take it from
/// here, so they cannot drift apart.
pub const WIDEST: u32 = 4;

/// The IPv6 blocks an address counts in, narrowest first, each with how
/// many times a per-address limit it may hold. The last, the widest,
/// holds the most ([`WIDEST`]).
const V6_LAYERS: [(u8, u32); 3] = [(64, 1), (56, 2), (48, WIDEST)];

/// One block an address counts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Block {
    /// The address with every bit past `prefix` cleared.
    network: IpAddr,
    prefix: u8,
    /// How many times a per-address limit it may hold (1 for an IPv4
    /// address; [`V6_LAYERS`] for IPv6).
    scale: u32,
}

impl Block {
    /// How many times a per-address limit this block may hold: 1 for an
    /// IPv4 address or an IPv6 `/64`, 2 for a `/56`, [`WIDEST`] for a
    /// `/48`.
    #[must_use]
    pub const fn scale(self) -> u32 {
        self.scale
    }

    /// `limit` for this block ([`Block::scale`]).
    #[must_use]
    pub fn limit(self, limit: usize) -> usize {
        limit.saturating_mul(usize::try_from(self.scale()).unwrap_or(1))
    }
}

impl fmt::Display for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// The blocks `address` counts in, the narrowest first: one for IPv4,
/// three for IPv6.
#[must_use]
pub fn blocks(address: IpAddr) -> Vec<Block> {
    match address {
        IpAddr::V4(v4) => vec![v4_block(v4)],
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => vec![v4_block(v4)],
            None => V6_LAYERS
                .into_iter()
                .map(|(prefix, scale)| Block {
                    network: IpAddr::V6(Ipv6Addr::from_bits(v6.to_bits() & v6_mask(prefix))),
                    prefix,
                    scale,
                })
                .collect(),
        },
    }
}

/// How many of something each block holds, against a per-address limit
/// scaled per block ([`Block::limit`]). A block holding nothing has no
/// entry, so the table holds only blocks in use.
#[derive(Debug, Default)]
pub struct BlockCounts(HashMap<Block, usize>);

impl BlockCounts {
    /// Whether one more fits in every one of `blocks` under `limit`.
    #[must_use]
    pub fn fits(&self, blocks: &[Block], limit: usize) -> bool {
        blocks
            .iter()
            .all(|block| self.0.get(block).copied().unwrap_or(0) < block.limit(limit))
    }

    /// One more in every one of `blocks`.
    pub fn add(&mut self, blocks: &[Block]) {
        for block in blocks {
            let count = self.0.entry(*block).or_insert(0);
            *count = count.saturating_add(1);
        }
    }

    /// One less in every one of `blocks`, forgetting a block at zero.
    pub fn remove(&mut self, blocks: &[Block]) {
        for block in blocks {
            if let Some(count) = self.0.get_mut(block) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    let _gone = self.0.remove(block);
                }
            }
        }
    }

    /// Whether no block holds anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

const fn v4_block(v4: Ipv4Addr) -> Block {
    Block {
        network: IpAddr::V4(v4),
        prefix: 32,
        scale: 1,
    }
}

/// The IPv6 mask of `prefix` leading ones (`prefix` at most 128).
fn v6_mask(prefix: u8) -> u128 {
    u128::MAX
        .checked_shl(128_u32.saturating_sub(u32::from(prefix)))
        .unwrap_or(0)
}

/// The IPv4 mask of `prefix` leading ones (`prefix` at most 32).
fn v4_mask(prefix: u8) -> u32 {
    u32::MAX
        .checked_shl(32_u32.saturating_sub(u32::from(prefix)))
        .unwrap_or(0)
}

/// A peer whose client-address header the relay believes: an address, or
/// a network in CIDR form (`10.0.0.0/8`, `fd00::/8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedProxy {
    network: IpAddr,
    prefix: u8,
}

impl TrustedProxy {
    /// Whether `peer` is this proxy (or in this network). An IPv4-mapped
    /// IPv6 peer is taken as its IPv4 address.
    #[must_use]
    pub fn contains(&self, peer: IpAddr) -> bool {
        let peer = match peer {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(peer, IpAddr::V4),
            IpAddr::V4(_) => peer,
        };
        match (self.network, peer) {
            (IpAddr::V4(network), IpAddr::V4(peer)) => {
                let mask = v4_mask(self.prefix);
                network.to_bits() & mask == peer.to_bits() & mask
            }
            (IpAddr::V6(network), IpAddr::V6(peer)) => {
                let mask = v6_mask(self.prefix);
                network.to_bits() & mask == peer.to_bits() & mask
            }
            (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// A trusted proxy that does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an address or a network in CIDR form (`192.0.2.7`, `10.0.0.0/8`, `fd00::/8`)")]
pub struct BadProxy;

impl FromStr for TrustedProxy {
    type Err = BadProxy;

    fn from_str(text: &str) -> Result<Self, BadProxy> {
        let (address, prefix) = match text.trim().split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text.trim(), None),
        };
        let network: IpAddr = address.parse().map_err(|_bad| BadProxy)?;
        let longest: u8 = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.parse::<u8>().map_err(|_bad| BadProxy)?,
            None => longest,
        };
        if prefix > longest {
            return Err(BadProxy);
        }
        Ok(Self { network, prefix })
    }
}

impl fmt::Display for TrustedProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn ipv6_counts_in_its_64_56_and_48_and_ipv4_as_itself() {
        let six = blocks(ip("2001:db8:aa:bbcc:1:2:3:4"));
        let shown: Vec<String> = six.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "2001:db8:aa:bbcc::/64",
                "2001:db8:aa:bb00::/56",
                "2001:db8:aa::/48"
            ]
        );
        assert_eq!(
            six.iter().map(|block| block.scale()).collect::<Vec<_>>(),
            [1, 2, 4]
        );
        assert_eq!(six[2].limit(16), 64);
        let four = blocks(ip("192.0.2.7"));
        assert_eq!(four.len(), 1);
        assert_eq!(four[0].to_string(), "192.0.2.7/32");
        // A mapped address is its IPv4 address.
        assert_eq!(blocks(ip("::ffff:192.0.2.7")), four);
        // Two /64s of one /48 share its block.
        assert_eq!(
            blocks(ip("2001:db8:aa:1::1"))[2],
            blocks(ip("2001:db8:aa:2::1"))[2]
        );
    }

    #[test]
    fn the_widest_block_holds_the_most() {
        let scales: Vec<u32> = V6_LAYERS.iter().map(|&(_prefix, scale)| scale).collect();
        assert!(
            scales.windows(2).all(|pair| pair[0] <= pair[1]),
            "{scales:?}"
        );
        assert_eq!(V6_LAYERS.last(), Some(&(48, WIDEST)));
        let widest = blocks(ip("2001:db8::1"))
            .iter()
            .map(|block| block.scale())
            .max();
        assert_eq!(widest, Some(WIDEST));
    }

    #[test]
    fn counts_hold_each_block_to_its_scaled_limit() {
        let mut counts = BlockCounts::default();
        // Two per /64: a /56 holds four, a /48 eight.
        let take = |counts: &mut BlockCounts, text: &str| {
            let blocks = blocks(ip(text));
            let fits = counts.fits(&blocks, 2);
            if fits {
                counts.add(&blocks);
            }
            fits
        };
        assert!(take(&mut counts, "2001:db8:1:1::1"));
        assert!(take(&mut counts, "2001:db8:1:1::2"));
        assert!(!take(&mut counts, "2001:db8:1:1::3"), "the /64's two");
        assert!(take(&mut counts, "2001:db8:1:2::1"));
        assert!(take(&mut counts, "2001:db8:1:2::2"));
        assert!(!take(&mut counts, "2001:db8:1:3::1"), "the /56's four");
        for other in [
            "2001:db8:1:101::1",
            "2001:db8:1:101::2",
            "2001:db8:1:102::1",
        ] {
            assert!(take(&mut counts, other), "{other}");
        }
        assert!(take(&mut counts, "2001:db8:1:102::2"));
        assert!(!take(&mut counts, "2001:db8:1:201::1"), "the /48's eight");
        assert!(take(&mut counts, "2001:db8:2::1"), "another /48");
        // Given back, every block is forgotten.
        for text in [
            "2001:db8:1:1::1",
            "2001:db8:1:1::2",
            "2001:db8:1:2::1",
            "2001:db8:1:2::2",
            "2001:db8:1:101::1",
            "2001:db8:1:101::2",
            "2001:db8:1:102::1",
            "2001:db8:1:102::2",
            "2001:db8:2::1",
        ] {
            counts.remove(&blocks(ip(text)));
        }
        assert!(counts.is_empty());
    }

    #[test]
    fn trusted_proxies_are_addresses_or_networks() {
        let one: TrustedProxy = "192.0.2.7".parse().unwrap();
        assert!(one.contains(ip("192.0.2.7")));
        assert!(one.contains(ip("::ffff:192.0.2.7")));
        assert!(!one.contains(ip("192.0.2.8")));
        let net: TrustedProxy = "10.0.0.0/8".parse().unwrap();
        assert!(net.contains(ip("10.200.3.4")));
        assert!(!net.contains(ip("11.0.0.1")));
        let six: TrustedProxy = "fd00::/8".parse().unwrap();
        assert!(six.contains(ip("fd12::1")));
        assert!(!six.contains(ip("10.0.0.1")));
        let all: TrustedProxy = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(ip("203.0.113.9")));
        for bad in ["", "proxy", "10.0.0.0/33", "fd00::/129", "10.0.0.0/x"] {
            assert_eq!(bad.parse::<TrustedProxy>(), Err(BadProxy), "{bad}");
        }
    }
}
