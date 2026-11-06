//! Shares of waiting reads (`session.read` with `wait`, protocol 6): at most
//! half of each connection's calls, and a few per origin on this node.
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

/// This node's waiting reads, by origin.
#[derive(Debug, Default)]
pub(crate) struct Waits {
    origins: Mutex<HashMap<Waiter, usize>>,
}

impl Waits {
    /// A place for one more waiting read of `waiter` on `connection`, if
    /// both shares have room; given back when the slot is dropped.
    pub(crate) fn take(
        self: &Arc<Self>,
        connection: &ConnectionWaits,
        waiter: Waiter,
    ) -> Option<WaitSlot> {
        let mut origins = self.origins.lock().unwrap_or_else(PoisonError::into_inner);
        let held = origins.get(&waiter).copied().unwrap_or(0);
        if held >= PER_ORIGIN {
            return None;
        }
        let _taken = connection
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
                taken.checked_add(1).filter(|&next| next <= PER_CONNECTION)
            })
            .ok()?;
        let _previous = origins.insert(waiter, held.saturating_add(1));
        Some(WaitSlot {
            waits: Arc::clone(self),
            connection: Arc::clone(connection),
            waiter,
        })
    }

    fn give_back(&self, waiter: Waiter) {
        let mut origins = self.origins.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = origins.get_mut(&waiter) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                let _gone = origins.remove(&waiter);
            }
        }
    }
}

/// A waiting read's place in its connection's and its origin's share.
#[derive(Debug)]
pub(crate) struct WaitSlot {
    waits: Arc<Waits>,
    connection: ConnectionWaits,
    waiter: Waiter,
}

impl Drop for WaitSlot {
    fn drop(&mut self) {
        let _previous =
            self.connection
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
                    taken.checked_sub(1)
                });
        self.waits.give_back(self.waiter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(session: SessionId) -> Waiter {
        Waiter {
            node: NodeId::random(),
            by: By::Agent(session),
        }
    }

    #[test]
    fn an_origin_holds_a_few_waits_and_a_connection_half_its_calls() {
        let waits = Arc::new(Waits::default());
        let connection = ConnectionWaits::default();
        let busy = agent(SessionId::random());
        let mut held: Vec<WaitSlot> = (0..PER_ORIGIN)
            .map(|_| waits.take(&connection, busy).unwrap())
            .collect();
        assert!(waits.take(&connection, busy).is_none(), "past its share");
        // Others share the rest of the connection's half.
        while held.len() < PER_CONNECTION {
            held.push(waits.take(&connection, agent(SessionId::random())).unwrap());
        }
        assert!(
            waits
                .take(&connection, agent(SessionId::random()))
                .is_none()
        );
        // Another connection has its own half.
        let other = ConnectionWaits::default();
        assert!(waits.take(&other, agent(SessionId::random())).is_some());
        // A slot given back is free again, for its origin too.
        drop(held.remove(0));
        assert!(waits.take(&connection, busy).is_some());
        drop(held);
        assert_eq!(connection.load(Ordering::Acquire), 0);
        assert!(waits.origins.lock().unwrap().is_empty());
    }
}
