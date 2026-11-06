//! Where a peer counts for the per-address limits ([`blocks`],
//! [`BlockCounts`]), and which peers may say where a client really is
//! ([`TrustedProxy`], [`ClientAddress`]). Public because a node counts
//! its direct route's unadmitted channels the same way, behind a reverse
//! proxy too (`vessel-node`'s secure gateway, the review's rounds 10 and
//! 11): one layering, one set of multipliers, one header parser.
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

use axum::http::{HeaderMap, HeaderName};

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

/// Who may say where a client really is: the header a reverse proxy
/// writes the client's address in, believed only from the proxies named.
/// The relay and a node's direct route both read clients through it.
#[derive(Debug, Clone, Default)]
pub struct ClientAddress {
    header: Option<HeaderName>,
    trusted: Vec<TrustedProxy>,
}

/// A client address header set with no proxy to believe it from: anyone
/// could pick their address and slip every per-address limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "a client address header is believed only from trusted proxies: name at least one \
     (--trusted-proxy)"
)]
pub struct NoTrustedProxy;

/// A trusted proxy's client address header that is not one bare address
/// ([`ClientAddress::of`]): answered `400`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the client address header is not one address")]
pub struct BadAddressHeader;

/// Where a request counts: the client's address, and whether it came
/// through a trusted proxy (by its header, or as the proxy itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Client {
    /// The address the per-address limits count.
    pub address: IpAddr,
    /// It came through a trusted proxy, whose own socket is not the
    /// client's.
    pub proxied: bool,
}

impl ClientAddress {
    /// `header` believed from `trusted`. A header needs a trusted proxy;
    /// trusted proxies without a header are ignored (a proxy that cannot
    /// say who its clients are is counted as any peer is).
    ///
    /// # Errors
    /// A header and no trusted proxy.
    pub fn new(
        header: Option<HeaderName>,
        trusted: Vec<TrustedProxy>,
    ) -> Result<Self, NoTrustedProxy> {
        match header {
            Some(_) if trusted.is_empty() => Err(NoTrustedProxy),
            Some(header) => Ok(Self {
                header: Some(header),
                trusted,
            }),
            None => Ok(Self::default()),
        }
    }

    /// Whether `peer` is a trusted proxy (never, without a header).
    #[must_use]
    pub fn is_trusted_proxy(&self, peer: IpAddr) -> bool {
        self.header.is_some() && self.trusted.iter().any(|proxy| proxy.contains(peer))
    }

    /// Where a request from `peer` with `headers` counts.
    ///
    /// From a trusted proxy the header must hold one bare address, IPv4
    /// (`192.0.2.7`) or IPv6 (`2001:db8::7`), with spaces around it at
    /// most: anything else (a list, a port, brackets, the header twice) is
    /// refused, since guessing which part the proxy wrote would let a
    /// client choose. A request from a trusted proxy without the header
    /// counts as the proxy itself. From anyone else the header is ignored.
    ///
    /// # Errors
    /// The trusted proxy's header is not one bare address.
    pub fn of(&self, peer: IpAddr, headers: &HeaderMap) -> Result<Client, BadAddressHeader> {
        let Some(name) = self
            .header
            .as_ref()
            .filter(|_name| self.is_trusted_proxy(peer))
        else {
            return Ok(Client {
                address: peer,
                proxied: false,
            });
        };
        let mut values = headers.get_all(name).iter();
        let address = match (values.next(), values.next()) {
            (None, _) => peer,
            (Some(value), None) => value
                .to_str()
                .ok()
                .and_then(|text| text.trim().parse::<IpAddr>().ok())
                .ok_or(BadAddressHeader)?,
            (Some(_first), Some(_second)) => return Err(BadAddressHeader),
        };
        Ok(Client {
            address,
            proxied: true,
        })
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
    fn a_trusted_proxys_header_is_one_bare_address() {
        let proxy = ip("127.0.0.1");
        let clients = ClientAddress::new(
            Some(HeaderName::from_static("x-real-ip")),
            vec!["127.0.0.1".parse().unwrap()],
        )
        .unwrap();
        let with = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                let _added = headers.append("x-real-ip", value.parse().unwrap());
            }
            headers
        };
        let client = |address: &str| Client {
            address: ip(address),
            proxied: true,
        };
        for good in ["203.0.113.1", " 2001:db8::1 ", "::ffff:192.0.2.1"] {
            assert_eq!(
                clients.of(proxy, &with(&[good])),
                Ok(client(good.trim())),
                "{good:?}"
            );
        }
        for bad in [
            "203.0.113.1, 10.0.0.1",
            "[2001:db8::1]:443",
            "203.0.113.1:443",
            "[2001:db8::1]",
            "",
        ] {
            assert_eq!(
                clients.of(proxy, &with(&[bad])),
                Err(BadAddressHeader),
                "{bad:?}"
            );
        }
        let twice = with(&["203.0.113.1", "203.0.113.2"]);
        assert_eq!(clients.of(proxy, &twice), Err(BadAddressHeader));
        // Without the header: the proxy itself, still proxied.
        assert_eq!(
            clients.of(proxy, &HeaderMap::new()),
            Ok(client("127.0.0.1"))
        );
        // From anyone else the header means nothing.
        let stranger = ip("198.51.100.9");
        assert_eq!(
            clients.of(stranger, &with(&["203.0.113.1"])),
            Ok(Client {
                address: stranger,
                proxied: false
            })
        );
        // A header needs a proxy; proxies without a header are ignored.
        let header = Some(HeaderName::from_static("x-real-ip"));
        assert_eq!(
            ClientAddress::new(header, Vec::new()).unwrap_err(),
            NoTrustedProxy
        );
        let ignored = ClientAddress::new(None, vec!["127.0.0.1".parse().unwrap()]).unwrap();
        assert!(!ignored.is_trusted_proxy(proxy));
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
