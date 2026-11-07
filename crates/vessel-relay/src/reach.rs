//! Which addresses a node's link may dial: any, for relays its owner
//! configured; or only public ones, for a fleet member dialing relays its
//! home named (`fleet.callback`).
//!
//! [Decision Log]
//! - Purpose: a home names the relays its members register with. A member
//!   must not be pointed at its own machine or private network (the
//!   review's round 6), and checking the host's text was not enough (round
//!   7: `127.1`, `2130706433`, `0x7f.0.0.1`, `[::ffff:0.0.0.0]`, CGNAT, a
//!   trailing dot, and names like `127.0.0.1.nip.io` that resolve to
//!   loopback all passed as public).
//! - Choice: the link resolves the relay's host when it dials and connects
//!   only to the addresses [`is_public`] accepts, to the address it
//!   checked (so a name cannot resolve one way for the check and another
//!   for the connection). The node's policy refuses non-canonical numeric
//!   hosts before that ([`numeric_host`]).
//! - Alternatives: classifying the host text only (misses names that
//!   resolve to private addresses); resolving once when the home names the
//!   relay (the answer can change before the dial).
//! - Trade-offs: "public" is a fixed list of special-purpose ranges, kept
//!   here; a relay inside a private network is reached only through the
//!   node's own `--relay`, or as the host the member reaches its home by.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Which addresses a link may connect to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Reach {
    /// Any the relay's host resolves to: a relay the node's owner chose.
    #[default]
    Any,
    /// Only public ones ([`is_public`]): a relay someone else named.
    Public,
}

/// Whether `address` is reachable on the internet at large: not this
/// machine, a private, shared (CGNAT), link-local or site-local network,
/// multicast, broadcast, or a reserved or unspecified range. IPv4 inside
/// IPv6 (mapped, compatible, NAT64) is judged as the IPv4 address.
#[must_use]
pub fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    let special = a == 0 // "this network"
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || (a == 100 && (b & 0xc0) == 64) // shared address space (CGNAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b & 0xfe) == 18) // benchmarking
        || address.is_multicast()
        || a >= 240; // reserved, and broadcast
    !special
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let [first, second, third, fourth, fifth, sixth, ..] = segments;
    // IPv4-mapped (::ffff:a.b.c.d), IPv4-compatible (::a.b.c.d, deprecated)
    // and NAT64 (64:ff9b::a.b.c.d): judged as the IPv4 address.
    let embedded = || {
        let [.., high, low] = segments;
        let [h1, h2] = high.to_be_bytes();
        let [l1, l2] = low.to_be_bytes();
        Ipv4Addr::new(h1, h2, l1, l2)
    };
    let zero_prefix = first == 0 && second == 0 && third == 0 && fourth == 0 && fifth == 0;
    if zero_prefix && (sixth == 0xffff || sixth == 0) {
        // `::` and `::1` fall here too, as 0.0.0.0 and 0.0.0.1: not public.
        return is_public_v4(embedded());
    }
    if first == 0x0064 && second == 0xff9b && third == 0 && fourth == 0 && fifth == 0 && sixth == 0
    {
        return is_public_v4(embedded());
    }
    let special = address.is_loopback()
        || address.is_unspecified()
        || (first & 0xfe00) == 0xfc00 // unique local
        || (first & 0xffc0) == 0xfe80 // link-local
        || (first & 0xffc0) == 0xfec0 // site-local (deprecated)
        || address.is_multicast();
    !special
}

/// Whether `host` (lowercase, without a trailing dot or brackets) is
/// written as a number, the way a URL parser reads it as IPv4: its last
/// label is decimal digits or `0x` hex. Such a host is only acceptable in
/// canonical dotted-quad form (`127.1`, `2130706433`, `0x7f.0.0.1` and
/// `0177.0.0.1` are not: each means 127.0.0.1 to some resolver).
#[must_use]
pub fn numeric_host(host: &str) -> bool {
    let last = host.rsplit('.').next().unwrap_or(host);
    !last.is_empty()
        && (last.bytes().all(|byte| byte.is_ascii_digit())
            || last
                .strip_prefix("0x")
                .is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(text: &str) -> bool {
        is_public(text.parse().unwrap())
    }

    #[test]
    fn local_private_and_special_addresses_are_not_public() {
        for local in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "127.255.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "100.100.100.100",
            "100.127.255.255",
            "192.0.0.8",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:0.0.0.0",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
            "::127.0.0.1",
            "64:ff9b::10.0.0.1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
        ] {
            assert!(!public(local), "{local}");
        }
        for open in [
            "1.1.1.1",
            "100.63.255.255",
            "100.128.0.1",
            "203.0.113.9",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
            "64:ff9b::1.1.1.1",
        ] {
            assert!(public(open), "{open}");
        }
    }

    #[test]
    fn hosts_written_as_numbers_are_recognised() {
        for numeric in [
            "127.1",
            "2130706433",
            "0x7f.0.0.1",
            "0177.0.0.1",
            "127.0.0.1",
            "1.2.3.0x4",
        ] {
            assert!(numeric_host(numeric), "{numeric}");
        }
        for name in ["relay.example", "127.0.0.1.nip.io", "localtest.me", "x0"] {
            assert!(!numeric_host(name), "{name}");
        }
    }
}
