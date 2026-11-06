//! Pairing invitations, held in the node's memory (plan decision 7).
//!
//! [Decision Log]
//! - Purpose: a pairing link is worth something only until it is used or
//!   expires (ADR 0010: ten minutes by default, at most a day, single use).
//! - Choice: a map from the SHA-256 of each invitation's secret to its label
//!   and expiry. Keyed by the hash, as ADR 0010 says, so a memory dump of a
//!   running node does not hand out live invitations. Expired entries are
//!   dropped whenever the map is touched, and at most [`MAX_PENDING`] wait
//!   at once, so a client with `admin` cannot grow it without bound.
//! - Not persisted: a restart voids pending invitations, which is acceptable
//!   for something that lives ten minutes (decision 7).
//! - Every failure to redeem looks the same to the caller (unknown, used or
//!   expired), so an attacker learns nothing from the difference.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sha2::{Digest as _, Sha256};
use vessel_proto::{InvitationSecret, Scope};

/// Invitations that may wait at once.
pub(crate) const MAX_PENDING: usize = 64;

/// The pending invitations.
#[derive(Debug, Default)]
pub(crate) struct Invitations {
    pending: HashMap<[u8; 32], Pending>,
}

#[derive(Debug)]
struct Pending {
    grant: Grant,
    expires: DateTime<Utc>,
}

/// What an invitation gives the device that redeems it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grant {
    /// The label chosen when inviting, if any.
    pub(crate) label: Option<String>,
    /// The scopes chosen when inviting (protocol 5), never empty.
    pub(crate) scopes: Vec<Scope>,
}

/// [`MAX_PENDING`] invitations are already waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Full;

impl Invitations {
    /// Remember a new invitation.
    pub(crate) fn add(
        &mut self,
        secret: &InvitationSecret,
        grant: Grant,
        expires: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), Full> {
        self.purge(now);
        if self.pending.len() >= MAX_PENDING {
            return Err(Full);
        }
        let _replaced = self.pending.insert(key(secret), Pending { grant, expires });
        Ok(())
    }

    /// Use up an invitation: `Some` if it was pending and has not expired,
    /// `None` otherwise. Either way it is gone afterwards.
    pub(crate) fn redeem(
        &mut self,
        secret: &InvitationSecret,
        now: DateTime<Utc>,
    ) -> Option<Grant> {
        self.purge(now);
        self.pending
            .remove(&key(secret))
            .filter(|pending| pending.expires > now)
            .map(|pending| pending.grant)
    }

    fn purge(&mut self, now: DateTime<Utc>) {
        self.pending.retain(|_key, pending| pending.expires > now);
    }
}

fn key(secret: &InvitationSecret) -> [u8; 32] {
    Sha256::digest(secret.expose()).into()
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn grant(label: Option<&str>) -> Grant {
        Grant {
            label: label.map(ToOwned::to_owned),
            scopes: Scope::DEVICE_DEFAULT.to_vec(),
        }
    }

    fn at(second: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000_i64.saturating_add(second), 0).unwrap()
    }

    #[test]
    fn an_invitation_is_good_once_and_only_until_it_expires() {
        let mut invitations = Invitations::default();
        let first = InvitationSecret::from_bytes([1; 16]);
        let second = InvitationSecret::from_bytes([2; 16]);
        invitations
            .add(&first, grant(Some("laptop")), at(600), at(0))
            .unwrap();
        invitations
            .add(&second, grant(None), at(600), at(0))
            .unwrap();

        assert_eq!(
            invitations.redeem(&first, at(10)),
            Some(grant(Some("laptop")))
        );
        assert_eq!(invitations.redeem(&first, at(11)), None, "single use");
        assert_eq!(invitations.redeem(&second, at(600)), None, "expired");
        assert_eq!(
            invitations.redeem(&InvitationSecret::from_bytes([3; 16]), at(1)),
            None
        );
    }

    #[test]
    fn pending_invitations_are_bounded_and_expired_ones_make_room() {
        let mut invitations = Invitations::default();
        for n in 0..MAX_PENDING {
            let secret = InvitationSecret::from_bytes([u8::try_from(n).unwrap(); 16]);
            invitations
                .add(&secret, grant(None), at(60), at(0))
                .unwrap();
        }
        let extra = InvitationSecret::from_bytes([200; 16]);
        assert_eq!(
            invitations.add(&extra, grant(None), at(60), at(1)),
            Err(Full)
        );
        let later = at(0) + TimeDelta::seconds(61);
        assert_eq!(invitations.add(&extra, grant(None), at(120), later), Ok(()));
    }
}
