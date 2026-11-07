//! Persistence of live sessions: one task per session drains its durable
//! channel into the store and publishes how far the log is committed.
//!
//! The task takes every record already waiting (up to [`BATCH`]) and
//! appends them in one call, so a burst of output costs one commit, not
//! one per record. Records stay `Arc`s all the way into the store.
//!
//! If the store fails, the session cannot keep its gap-free log, so the
//! task kills it and marks its durability [`Durability::Broken`]; readers
//! waiting for those records get an error instead of a silent hole.
//!
//! The task reports how it ended ([`Persisted`]); `sessions` then hands the
//! session over to the store and removes it from the runtime.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use vessel_proto::{EventRecord, EventSeq, SessionId};
use vessel_session::SessionHandle;
use vessel_store::Store;

/// Most records appended in one store call.
const BATCH: usize = 256;

/// How much of a live session's log is committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Durability {
    /// Every record before this sequence number is committed.
    Through(EventSeq),
    /// Persisting failed; the session was killed and later records are not
    /// in the store.
    Broken,
}

/// How persisting a session's log ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Persisted {
    /// Every record is committed, through the final status: the durable
    /// channel was drained to its end, which means the session ended.
    Complete,
    /// The store failed; the session was killed and may still be ending.
    Incomplete,
}

/// The log of `session` could not be persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the log of session {0} could not be persisted")]
pub(crate) struct Unpersisted(pub(crate) SessionId);

/// Drain `durable` into the store until the session ends.
pub(crate) async fn run(
    store: Store,
    handle: SessionHandle,
    mut durable: mpsc::Receiver<Arc<EventRecord>>,
    durability: watch::Sender<Durability>,
) -> Persisted {
    let session = handle.id();
    let mut batch = Vec::with_capacity(BATCH);
    while durable.recv_many(&mut batch, BATCH).await > 0 {
        // The runtime never hands out the last `u64` (it stops the session
        // instead), so this only guards against a broken invariant; the log
        // then counts as incomplete.
        let Some(next) = batch.last().and_then(|record| record.seq.next()) else {
            tracing::error!(%session, "record sequence exhausted; stopping persistence");
            durability.send_modify(|durability| *durability = Durability::Broken);
            handle.kill();
            return Persisted::Incomplete;
        };
        let records = std::mem::replace(&mut batch, Vec::with_capacity(BATCH));
        if let Err(error) = store.append(session, records).await {
            tracing::error!(%session, %error, "cannot persist session records; killing the session");
            durability.send_modify(|durability| *durability = Durability::Broken);
            handle.kill();
            // Dropping `durable` here lets the session finish without
            // waiting for a store that cannot take its records.
            return Persisted::Incomplete;
        }
        durability.send_modify(|durability| *durability = Durability::Through(next));
    }
    tracing::debug!(%session, "session log fully persisted");
    Persisted::Complete
}

/// Wait until every record before `target` is committed.
pub(crate) async fn wait_until(
    session: SessionId,
    durability: &mut watch::Receiver<Durability>,
    target: EventSeq,
) -> Result<(), Unpersisted> {
    let reached = |durability: &Durability| match durability {
        Durability::Through(next) => *next >= target,
        Durability::Broken => true,
    };
    let waited = durability.wait_for(reached).await.map(|current| *current);
    // An error means the persist task is gone; its last word stands.
    let last = waited.unwrap_or_else(|_finished| *durability.borrow());
    match last {
        Durability::Through(next) if next >= target => Ok(()),
        Durability::Through(_) | Durability::Broken => Err(Unpersisted(session)),
    }
}
