//! Registration tickets (protocol 6, `fleet.callback`): one node key may
//! register with a relay until a time, without the relay's token.
//!
//! [Decision Log]
//! - Purpose: a fleet member with no route of its own registers with its
//!   home's relay, so the home can call it back (Track S's fresh-machine
//!   smoke). Handing members the relay's token (103d765, the review's
//!   round 6) let any `sync` device register any key, for good, and kept
//!   working after the device was revoked.
//! - Constraint: the relay keeps no configuration but its token, and no
//!   state but who is registered now (ADR 0009); a node proves it holds its
//!   key before it is registered.
//! - Alternatives: the token itself (any key, never expires); a signature
//!   by the home's key (the relay would need to know which homes it
//!   trusts); a list of allowed keys on the relay (state it does not keep);
//!   the home calling members back over their own connection to it (no
//!   relay secret at all, but the secure channel's roles reversed: larger,
//!   see the plan).
//! - Choice: a ticket names one node key and an expiry and carries
//!   HMAC-SHA256 over both, keyed with the relay's token, under a domain
//!   label. The relay checks it with the token it has, against the key the
//!   node then proves it holds, and ends a registration made with it when
//!   it expires ([`MAX_TICKET_LIFETIME`] at most ahead of the relay's
//!   clock).
//! - Trade-offs: whoever holds the token mints tickets (the operator, and
//!   a home that registers with the relay, as before); a ticket is a bearer
//!   credential for one key, of no use without that key's private half;
//!   rotating the token voids every ticket at once.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;
use vessel_proto::{NodePublicKey, RelayTicket, RelayToken, TicketMac};

/// Prefixed to what a ticket's MAC covers, so the token's MAC over these
/// bytes is never one made for anything else.
const DOMAIN: &[u8] = b"vessel relay ticket v1\0";

/// The furthest ahead of the relay's clock a ticket may expire: a home
/// mints them for a day; one reaching further, minted wrong or by a clock
/// far ahead, is refused rather than registering a node for long.
pub const MAX_TICKET_LIFETIME: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// Why a ticket does not register a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TicketError {
    /// It names another key than the one registering.
    #[error("the ticket is for another node key")]
    OtherKey,
    /// Its time has passed.
    #[error("the ticket has expired")]
    Expired,
    /// It expires further ahead than [`MAX_TICKET_LIFETIME`].
    #[error("the ticket expires too far ahead")]
    TooLong,
    /// Its MAC is not the relay token's.
    #[error("the ticket was not made with this relay's token")]
    Forged,
}

/// A ticket that registers `key` until `expires_at` (seconds since the
/// Unix epoch), made with the relay's `token`.
#[must_use]
pub fn issue_ticket(token: &RelayToken, key: &NodePublicKey, expires_at: u64) -> RelayTicket {
    let mut bytes = [0_u8; 32];
    if let Some(mac) = mac(token, key, expires_at) {
        bytes.copy_from_slice(&mac.finalize().into_bytes());
    }
    // Without a MAC (HMAC takes keys of any length, so never) the ticket
    // is all zeros, which no relay accepts.
    RelayTicket {
        key: *key,
        expires_at,
        mac: TicketMac::from_bytes(bytes),
    }
}

/// Whether `ticket` registers the node holding `key` at `now` (seconds
/// since the Unix epoch) with the relay whose token is `token`. The MAC is
/// compared in constant time.
///
/// # Errors
/// The ticket is for another key, has expired, reaches too far ahead, or
/// was not made with `token`.
pub fn check_ticket(
    token: &RelayToken,
    ticket: &RelayTicket,
    key: &NodePublicKey,
    now: u64,
) -> Result<(), TicketError> {
    if ticket.key != *key {
        return Err(TicketError::OtherKey);
    }
    if ticket.expires_at <= now {
        return Err(TicketError::Expired);
    }
    if ticket.expires_at > now.saturating_add(MAX_TICKET_LIFETIME.as_secs()) {
        return Err(TicketError::TooLong);
    }
    mac(token, key, ticket.expires_at)
        .ok_or(TicketError::Forged)?
        .verify_slice(ticket.mac.expose())
        .map_err(|_mismatch| TicketError::Forged)
}

/// Seconds since the Unix epoch now (0 for a clock before it).
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The MAC state over a ticket's fields; `None` only if HMAC refused the
/// key, which it never does.
fn mac(token: &RelayToken, key: &NodePublicKey, expires_at: u64) -> Option<Hmac<Sha256>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(token.expose().as_bytes()).ok()?;
    mac.update(DOMAIN);
    mac.update(key.as_bytes());
    mac.update(&expires_at.to_be_bytes());
    Some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn token(text: &str) -> RelayToken {
        RelayToken::new(text).unwrap()
    }

    #[test]
    fn a_ticket_registers_its_own_key_until_it_expires_and_nothing_else() {
        let key = NodePublicKey::from_bytes([3; 32]);
        let ticket = issue_ticket(&token("relay"), &key, NOW + 60);
        assert_eq!(check_ticket(&token("relay"), &ticket, &key, NOW), Ok(()));
        // Another key, even one the holder proves, is not registered.
        let other = NodePublicKey::from_bytes([4; 32]);
        assert_eq!(
            check_ticket(&token("relay"), &ticket, &other, NOW),
            Err(TicketError::OtherKey)
        );
        // Changing the key or the time breaks the MAC.
        let mut moved = ticket.clone();
        moved.key = other;
        assert_eq!(
            check_ticket(&token("relay"), &moved, &other, NOW),
            Err(TicketError::Forged)
        );
        let mut later = ticket.clone();
        later.expires_at = NOW + 120;
        assert_eq!(
            check_ticket(&token("relay"), &later, &key, NOW),
            Err(TicketError::Forged)
        );
        // Expired, and a rotated token voids it.
        assert_eq!(
            check_ticket(&token("relay"), &ticket, &key, NOW + 60),
            Err(TicketError::Expired)
        );
        assert_eq!(
            check_ticket(&token("rotated"), &ticket, &key, NOW),
            Err(TicketError::Forged)
        );
    }

    #[test]
    fn a_ticket_reaching_too_far_ahead_is_refused() {
        let key = NodePublicKey::from_bytes([3; 32]);
        let limit = NOW + MAX_TICKET_LIFETIME.as_secs();
        let at_limit = issue_ticket(&token("relay"), &key, limit);
        assert_eq!(check_ticket(&token("relay"), &at_limit, &key, NOW), Ok(()));
        let beyond = issue_ticket(&token("relay"), &key, limit + 1);
        assert_eq!(
            check_ticket(&token("relay"), &beyond, &key, NOW),
            Err(TicketError::TooLong)
        );
    }
}
