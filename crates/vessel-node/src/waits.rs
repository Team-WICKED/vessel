//! Shares of waiting reads (`session.read` with `wait`, protocol 6): at most
//! half of each connection's calls, a few per origin on this node, and a
//! few more for everything one other node, or one relaying member device,
//! asks.
//!
//! [Decision Log] (the review of Track X, 2026-11-06, item 3)
//! - Problem: a waiting read holds one of its connection's calls for up to
//!   a minute, and `vessel-rpc` runs 64 calls at once per connection. The
//!   home reaches each member over one connection, and a member reaches
//!   its home over one (the pull loop's, which also carries `fleet.watch`
//!   and relayed calls): 64 waits through either made every other call on
//!   it fail with "too many calls in flight", fleet sync included.
//! - Choice: waiting reads may hold at most [`PER_CONNECTION`] (half) of a
//!   connection's calls, and one origin (an owner, a device or an agent,
//!   of this node or of the node a delivered call came from) at most
//!   [`PER_ORIGIN`] on this node. Past either share a read is answered at
//!   once, as without `wait`, and a waiting read passed on to another node
//!   goes without its wait, so it holds no call on any hop for long. The
//!   other half of every connection is always there for everything else,
//!   and one busy origin cannot take a shared connection's half alone.
//! - Round 2 of the review: an origin's agent session is whatever the
//!   delivering node says it is, so a relaying member could name a new
//!   session for every read and take a shared connection's whole half
//!   alone (40 reads made 32 waits, and the home's owner's own waits came
//!   back at once). So every origin of one other node shares [`PER_NODE`]
//!   on this node, and every call one relaying member device passes on
//!   through the home shares [`PER_RELAY`], whatever origins they name.
//! - Alternatives: a connection (or a pool) of its own for long polls
//!   between nodes: one more authenticated link per pair to keep up, and
//!   the caller's own connection to its node would still fill. Refusing a
//!   read past the share: the agent's tool would fail where a poll works.
//! - Cost: past a share, a waiter polls instead of waiting. A client sees
//!   an empty page before its wait could have ended and should pause
//!   before asking again (`vessel mcp` pauses a second), so that is about
//!   a poll a second per waiter, not a busy loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use vessel_proto::{DeviceId, NodeId, Origin, OriginBy, SessionId};

use crate::principal::Principal;

/// Waiting reads one connection may hold: half of `vessel-rpc`'s 64 calls
/// in flight per connection (`ServerConfig::max_in_flight`, which the node
/// leaves at its default).
pub(crate) const PER_CONNECTION: usize = 32;

/// Waiting reads one origin may hold on this node at once.
pub(crate) const PER_ORIGIN: usize = 8;

/// Waiting reads every origin of one other node may hold on this node.
pub(crate) const PER_NODE: usize = 16;

/// Waiting reads one relaying member device may pass on through the home.
pub(crate) const PER_RELAY: usize = 16;

/// A share of waiting reads, counted on this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Share {
    /// One origin.
    Origin(Waiter),
    /// Every origin of another node (a delivered call names its session as
    /// the delivering node says).
    Node(NodeId),
    /// Every call one relaying member device passes on, whatever origins
    /// they name.
    Relay(DeviceId),
}

impl Share {
    const fn cap(self) -> usize {
        match self {
            Self::Origin(_) => PER_ORIGIN,
            Self::Node(_) => PER_NODE,
            Self::Relay(_) => PER_RELAY,
        }
    }
}

/// Who waits, for the share of each origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Waiter {
    node: NodeId,
    by: By,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum By {
    Owner,
    Device(DeviceId),
    Agent(SessionId),
}

impl Waiter {
    /// The waiter `principal` is, on node `here` (a delivered call's is its
    /// origin).
    pub(crate) fn of(principal: &Principal, here: NodeId) -> Self {
        match principal {
            Principal::Owner => Self {
                node: here,
                by: By::Owner,
            },
            Principal::Device { id, .. } => Self {
                node: here,
                by: By::Device(*id),
            },
            Principal::Agent { node, session, .. } => Self {
                node: *node,
                by: By::Agent(*session),
            },
            Principal::Delegated { origin, .. } => Self::origin(origin),
        }
    }

    /// The shares a read of this waiter counts against on node `here`: its
    /// own, and, for an origin on another node, that node's.
    pub(crate) fn shares(self, here: NodeId) -> Vec<Share> {
        let mut shares = vec![Share::Origin(self)];
        if self.node != here {
            shares.push(Share::Node(self.node));
        }
        shares
    }

    /// The waiter a call from `origin` speaks for.
    pub(crate) const fn origin(origin: &Origin) -> Self {
        Self {
            node: origin.node,
            by: match origin.by {
                OriginBy::Owner => By::Owner,
                OriginBy::Device { id } => By::Device(id),
                OriginBy::Agent { session, .. } => By::Agent(session),
            },
        }
    }
}

/// One connection's waiting reads (shared by its clones, `acting_as`).
pub(crate) type ConnectionWaits = Arc<AtomicUsize>;

/// This node's waiting reads, by share.
#[derive(Debug, Default)]
pub(crate) struct Waits {
    held: Mutex<HashMap<Share, usize>>,
}

impl Waits {
    /// A place for one more waiting read on `connection` in each of
    /// `shares`, if the connection's half and every share have room; given
    /// back when the slot is dropped.
    pub(crate) fn take(
        self: &Arc<Self>,
        connection: &ConnectionWaits,
        shares: Vec<Share>,
    ) -> Option<WaitSlot> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if shares
            .iter()
            .any(|share| held.get(share).copied().unwrap_or(0) >= share.cap())
        {
            return None;
        }
        let _taken = connection
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
                taken.checked_add(1).filter(|&next| next <= PER_CONNECTION)
            })
            .ok()?;
        for share in &shares {
            let count = held.entry(*share).or_insert(0);
            *count = count.saturating_add(1);
        }
        Some(WaitSlot {
            waits: Arc::clone(self),
            connection: Arc::clone(connection),
            shares,
        })
    }

    fn give_back(&self, shares: &[Share]) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        for share in shares {
            if let Some(count) = held.get_mut(share) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    let _gone = held.remove(share);
                }
            }
        }
    }
}

/// A waiting read's place in its connection's half and in its shares.
#[derive(Debug)]
pub(crate) struct WaitSlot {
    waits: Arc<Waits>,
    connection: ConnectionWaits,
    shares: Vec<Share>,
}

impl Drop for WaitSlot {
    fn drop(&mut self) {
        let _previous =
            self.connection
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
                    taken.checked_sub(1)
                });
        self.waits.give_back(&self.shares);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(node: NodeId) -> Waiter {
        Waiter {
            node,
            by: By::Agent(SessionId::random()),
        }
    }

    #[test]
    fn an_origin_holds_a_few_waits_and_a_connection_half_its_calls() {
        let here = NodeId::random();
        let waits = Arc::new(Waits::default());
        let connection = ConnectionWaits::default();
        let take = |waiter: Waiter| waits.take(&connection, waiter.shares(here));
        let busy = agent(here);
        let mut held: Vec<WaitSlot> = (0..PER_ORIGIN).map(|_| take(busy).unwrap()).collect();
        assert!(take(busy).is_none(), "past its share");
        // Others share the rest of the connection's half.
        while held.len() < PER_CONNECTION {
            held.push(take(agent(here)).unwrap());
        }
        assert!(take(agent(here)).is_none());
        // Another connection has its own half.
        let other = ConnectionWaits::default();
        assert!(waits.take(&other, agent(here).shares(here)).is_some());
        // A slot given back is free again, for its origin too.
        drop(held.remove(0));
        assert!(take(busy).is_some());
        drop(held);
        assert_eq!(connection.load(Ordering::Acquire), 0);
        assert!(waits.held.lock().unwrap().is_empty());
    }

    /// Round 2 of the review: a node (or a relaying device) naming a new
    /// origin for every read still has one share on this node.
    #[test]
    fn a_node_or_a_relaying_device_has_one_share_whatever_origins_it_names() {
        let here = NodeId::random();
        let waits = Arc::new(Waits::default());
        let (from_member, from_home) = (ConnectionWaits::default(), ConnectionWaits::default());
        let member = NodeId::random();
        let held: Vec<WaitSlot> = (0..PER_NODE)
            .map(|_| waits.take(&from_home, agent(member).shares(here)).unwrap())
            .collect();
        assert!(waits.take(&from_home, agent(member).shares(here)).is_none());
        // Another node's origins still wait on the same connection.
        assert!(
            waits
                .take(&from_home, agent(NodeId::random()).shares(here))
                .is_some()
        );
        drop(held);

        // The home passing reads on for one relaying device.
        let device = DeviceId::random();
        let relayed = |waits: &Arc<Waits>| {
            let mut shares = agent(member).shares(here);
            shares.push(Share::Relay(device));
            waits.take(&from_member, shares)
        };
        let held: Vec<WaitSlot> = (0..PER_RELAY).map(|_| relayed(&waits).unwrap()).collect();
        assert!(relayed(&waits).is_none());
        drop(held);
        assert!(waits.held.lock().unwrap().is_empty());
    }
}
