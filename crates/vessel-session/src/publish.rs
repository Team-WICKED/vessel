//! [`Publisher`]: numbers events and delivers them to the store and to
//! followers. Used only by the session actor.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, watch};
use vessel_proto::{
    AgentEvent, EventRecord, EventSeq, HarnessSessionId, ProfileName, SessionEvent, SessionInfo,
    SessionStatus,
};

use crate::handle::Shared;

/// The actor's write side of a session.
///
/// Publishing is two steps so the actor can stay responsive while the store
/// is slow: [`slot`](Self::slot) waits for room in the durable channel
/// (cancel-safe, so it can sit in a `select!` next to `kill`), and
/// [`commit`](Self::commit) then numbers the event and delivers it
/// everywhere without waiting.
#[derive(Debug)]
pub(crate) struct Publisher {
    shared: Arc<Shared>,
    /// `None` once the node dropped its receiver.
    durable: Option<mpsc::Sender<Arc<EventRecord>>>,
    info: watch::Sender<SessionInfo>,
    next: EventSeq,
}

/// Room for one record in the durable channel.
#[derive(Debug)]
pub(crate) enum Slot {
    Durable(mpsc::OwnedPermit<Arc<EventRecord>>),
    /// The node no longer receives records (it dropped the channel).
    Gone,
}

impl Publisher {
    pub(crate) const fn new(
        shared: Arc<Shared>,
        durable: mpsc::Sender<Arc<EventRecord>>,
        info: watch::Sender<SessionInfo>,
    ) -> Self {
        Self {
            shared,
            durable: Some(durable),
            info,
            next: EventSeq::ZERO,
        }
    }

    /// Wait for room in the durable channel. Cancel-safe.
    pub(crate) async fn slot(&self) -> Slot {
        let Some(durable) = &self.durable else {
            return Slot::Gone;
        };
        // An owned permit (on a cloned sender, one atomic increment) so the
        // permit does not borrow `self` while `commit` mutates it.
        match durable.clone().reserve_owned().await {
            Ok(permit) => Slot::Durable(permit),
            Err(_closed) => Slot::Gone,
        }
    }

    /// Number `event`, hand it to the store, append it to the ring,
    /// broadcast it, and update the session info, in that order.
    ///
    /// The info is updated last, so a reader that sees `next_seq = N` can
    /// always `follow` up to `N`.
    pub(crate) fn commit(&mut self, slot: Slot, event: SessionEvent) {
        let seq = self.next;
        // The last `u64` is never handed out, so `next_seq` stays
        // representable. Unreachable in practice (2^64 events).
        let Some(next) = seq.next() else {
            tracing::error!(session = %self.shared.id, "event sequence exhausted; dropping event");
            self.shared.kill.cancel();
            return;
        };
        let at = Utc::now();
        let change = InfoChange::of(&event);
        // The one allocation of this record; the store, the ring and every
        // follower share it.
        let record = Arc::new(EventRecord { seq, at, event });
        match slot {
            Slot::Durable(permit) => drop(permit.send(Arc::clone(&record))),
            Slot::Gone => {
                if self.durable.take().is_some() {
                    tracing::warn!(
                        session = %self.shared.id,
                        "durable receiver dropped; records are no longer persisted"
                    );
                }
            }
        }
        self.shared.ring().append(record);
        self.next = next;
        let _notified = self.info.send_if_modified(|info| {
            info.next_seq = next;
            change.apply(info, at)
        });
    }
}

/// The session is over once its actor drops the publisher: live followers
/// are told (their receivers close), as are info watchers (the `watch`
/// sender drops with it) and the node (the durable sender drops with it).
impl Drop for Publisher {
    fn drop(&mut self) {
        self.shared.ring().close();
    }
}

/// What an event changes in [`SessionInfo`] besides `next_seq`.
enum InfoChange {
    None,
    Status(SessionStatus),
    HarnessSession(HarnessSessionId),
    Profile(Option<ProfileName>),
}

impl InfoChange {
    fn of(event: &SessionEvent) -> Self {
        match event {
            SessionEvent::Status { status } => Self::Status(status.clone()),
            SessionEvent::Agent {
                event: AgentEvent::Ready { harness_session },
            } => Self::HarnessSession(harness_session.clone()),
            SessionEvent::ProfileChanged { to, .. } => Self::Profile(to.clone()),
            SessionEvent::Terminal(_) | SessionEvent::Agent { .. } => Self::None,
        }
    }

    /// Apply to `info`; returns whether watchers should be woken.
    fn apply(self, info: &mut SessionInfo, at: DateTime<Utc>) -> bool {
        match self {
            Self::None => false,
            Self::Status(status) => {
                if !status.is_live() {
                    info.ended_at = Some(at);
                }
                info.status = status;
                true
            }
            Self::HarnessSession(id) => {
                info.harness_session = Some(id);
                true
            }
            Self::Profile(profile) => {
                info.profile = profile;
                true
            }
        }
    }
}
