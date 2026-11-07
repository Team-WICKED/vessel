//! The node's sessions: live ones in the runtime, and every one ever
//! stored, including those from earlier runs.
//!
//! A session created in this run is "live" here while the runtime has it:
//! from its creation until its whole log, final status included, is
//! committed. Then it is handed over: removed from the runtime (which frees
//! its ring) and from the durability table, and served from the store like
//! a session of an earlier run. Stored sessions can be listed, read and
//! followed, and refuse input with `Conflict`. Handles and subscriptions
//! taken before the hand-over keep working (the runtime guarantees that),
//! and every record they could still need is in the store by then.
//!
//! The only state this type adds to the runtime's is each live session's
//! durability watch, kept in a small id-keyed table that is written once per
//! session and otherwise only read (lock held for a lookup, never across an
//! `.await`).
//!
//! That table is also what makes a runtime session visible. The runtime
//! registers a session before `create` returns, and the store row is
//! written after; a session is listed and found only once it has a table
//! entry, which is inserted before the store row is awaited. So a session
//! is never listed without being findable, and never found as a stored
//! (ended) session while it runs.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use tokio::sync::watch;
use tokio_util::task::TaskTracker;
use vessel_proto::{
    EventPage, EventRecord, EventSeq, SessionId, SessionInfo, SessionInput, SessionSpec,
};
use vessel_session::{
    Created, FollowError, LaunchOptions, ProfileMove, SessionHandle, SessionRuntime,
};
use vessel_store::Store;

use crate::agents::Agents;
use crate::error::ServiceError;
use crate::persist::{self, Durability, Persisted};

/// Where a session was found.
#[expect(
    clippy::large_enum_variant,
    reason = "returned once per lookup and matched at once, never queued or \
              stored; boxing would add an allocation to every lookup"
)]
pub(crate) enum Found {
    /// Created in this run and not yet handed over to the store.
    Live {
        handle: SessionHandle,
        durability: watch::Receiver<Durability>,
    },
    /// Ended, and served from the store.
    Stored(SessionInfo),
}

/// Live and stored sessions behind one interface.
#[derive(Debug)]
pub(crate) struct Sessions {
    runtime: Arc<SessionRuntime>,
    store: Store,
    durability: RwLock<HashMap<SessionId, watch::Receiver<Durability>>>,
    /// Session creations and persist tasks; shutdown waits for both.
    tasks: TaskTracker,
    /// Sessions' agents: a session's credential is retired when it ends.
    agents: Arc<Agents>,
}

impl Sessions {
    pub(crate) fn new(
        runtime: Arc<SessionRuntime>,
        store: Store,
        tasks: TaskTracker,
        agents: Arc<Agents>,
    ) -> Self {
        Self {
            runtime,
            store,
            durability: RwLock::new(HashMap::new()),
            tasks,
            agents,
        }
    }

    pub(crate) const fn store(&self) -> &Store {
        &self.store
    }

    /// Start a session and its persistence.
    ///
    /// Runs as its own task: once the process is started, storing the
    /// session and starting its persistence must finish even if the client
    /// cancels the call, or the session would run unrecorded.
    pub(crate) async fn create(
        self: &Arc<Self>,
        spec: SessionSpec,
        options: LaunchOptions,
    ) -> Result<SessionInfo, ServiceError> {
        let sessions = Arc::clone(self);
        self.tasks
            .spawn(async move { sessions.start(spec, options).await })
            .await
            .map_err(ServiceError::Task)?
    }

    async fn start(
        self: &Arc<Self>,
        spec: SessionSpec,
        options: LaunchOptions,
    ) -> Result<SessionInfo, ServiceError> {
        let Created { handle, durable } = self.runtime.create_with(spec, options).await?;
        let id = handle.id();
        self.agents.started(id, handle.clone());
        // The agent's credential dies with the session (ADR 0013).
        let ending = handle.clone();
        let agents = Arc::clone(&self.agents);
        drop(self.tasks.spawn(async move {
            let _final = ending.ended().await;
            agents.retire(id);
        }));
        // Visible from here on (see the module docs). Readers that need the
        // log wait on the watch until persistence starts below.
        let (durability, watching) = watch::channel(Durability::Through(EventSeq::ZERO));
        let previous = self
            .durability
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, watching);
        if previous.is_some() {
            tracing::error!(session = %id, "session id created twice");
        }
        if let Err(error) = self.store.create_session(handle.info()).await {
            // Without a stored row its records cannot be kept. Fail anyone
            // already waiting for its log, stop it, and forget it; dropping
            // `durable` lets it finish without waiting for us.
            tracing::error!(session = %id, %error, "cannot store a new session; killing it");
            durability.send_modify(|durability| *durability = Durability::Broken);
            handle.kill();
            self.forget(id);
            drop(durable);
            // Remove it from the runtime once it has ended, without holding
            // up this call's answer.
            let sessions = Arc::clone(self);
            drop(self.tasks.spawn(async move {
                sessions.hand_over(handle, Persisted::Incomplete).await;
            }));
            return Err(error.into());
        }
        let info = handle.info();
        let sessions = Arc::clone(self);
        let store = self.store.clone();
        drop(self.tasks.spawn(async move {
            let persisted = persist::run(store, handle.clone(), durable, durability).await;
            sessions.hand_over(handle, persisted).await;
        }));
        Ok(info)
    }

    /// The session's log is complete in the store (or never will be): let
    /// the store serve it, and free what the runtime kept for it.
    ///
    /// A complete log means the durable channel was drained to its end, so
    /// the session has ended and `remove` accepts it. Otherwise persistence
    /// stopped early and killed the session; it is removed once it has
    /// ended.
    async fn hand_over(&self, handle: SessionHandle, persisted: Persisted) {
        let id = handle.id();
        if persisted == Persisted::Incomplete {
            let _final = handle.ended().await;
        }
        if let Err(error) = self.runtime.remove(id) {
            tracing::warn!(session = %id, %error, "cannot remove a finished session from the runtime");
        }
        self.forget(id);
    }

    /// Drop a session's durability entry; it is then found in the store.
    fn forget(&self, id: SessionId) {
        drop(
            self.durability
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id),
        );
    }

    pub(crate) async fn find(&self, id: SessionId) -> Result<Found, ServiceError> {
        let durability = self
            .durability
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned();
        if let Some(durability) = durability
            && let Some(handle) = self.runtime.get(id)
        {
            return Ok(Found::Live { handle, durability });
        }
        match self.store.session(id).await? {
            Some(info) => Ok(Found::Stored(info)),
            None => Err(ServiceError::NotFound(id)),
        }
    }

    /// Every session, newest first. Live sessions report the runtime's view,
    /// which is ahead of the store's; runtime sessions without a table entry
    /// (still being created, or failed to store) are left out, so every
    /// listed session can be found.
    pub(crate) async fn list(&self) -> Result<Vec<SessionInfo>, ServiceError> {
        let mut all: HashMap<SessionId, SessionInfo> = self
            .store
            .sessions()
            .await?
            .into_iter()
            .map(|info| (info.id, info))
            .collect();
        let visible: Vec<SessionInfo> = {
            let durability = self
                .durability
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            self.runtime
                .list()
                .into_iter()
                .filter(|info| durability.contains_key(&info.id))
                .collect()
        };
        for info in visible {
            drop(all.insert(info.id, info));
        }
        let mut list: Vec<SessionInfo> = all.into_values().collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(list)
    }

    pub(crate) async fn get(&self, id: SessionId) -> Result<SessionInfo, ServiceError> {
        Ok(match self.find(id).await? {
            Found::Live { handle, .. } => handle.info(),
            Found::Stored(info) => info,
        })
    }

    pub(crate) async fn send(
        &self,
        id: SessionId,
        input: SessionInput,
    ) -> Result<(), ServiceError> {
        match self.find(id).await? {
            Found::Live { handle, .. } => Ok(handle.send(input).await?),
            Found::Stored(_) => Err(ServiceError::Ended(id)),
        }
    }

    /// Move a live session to another profile (`profiles::moving` plans it).
    pub(crate) async fn move_profile(
        &self,
        id: SessionId,
        plan: impl AsyncFnOnce(&SessionInfo) -> Result<ProfileMove, ServiceError>,
    ) -> Result<SessionInfo, ServiceError> {
        match self.find(id).await? {
            Found::Live { handle, .. } => {
                let request = plan(&handle.info()).await?;
                Ok(self.runtime.move_profile(id, request).await?)
            }
            Found::Stored(_) => Err(ServiceError::Ended(id)),
        }
    }

    /// Stop a session. Killing one that already ended is a no-op.
    pub(crate) async fn kill(&self, id: SessionId) -> Result<(), ServiceError> {
        match self.find(id).await? {
            Found::Live { handle, .. } => handle.kill(),
            Found::Stored(_) => {}
        }
        Ok(())
    }

    /// A page of the log. For a live session, first waits until everything
    /// the session had produced when the call arrived is committed, so the
    /// page (and `at_end`) reflects the log as of the call, not the store's
    /// lag; and records retention already pruned from the store but still
    /// in the session's ring come from the ring (see [`with_ring`]).
    pub(crate) async fn read(
        &self,
        id: SessionId,
        from: EventSeq,
        limit: u32,
    ) -> Result<EventPage, ServiceError> {
        match self.find(id).await? {
            Found::Live {
                handle,
                mut durability,
            } => {
                let produced = handle.info().next_seq;
                persist::wait_until(id, &mut durability, produced).await?;
                let page = self.store.read_events(id, from, limit).await?;
                Ok(with_ring(page, &handle, from, limit))
            }
            Found::Stored(_) => Ok(self.store.read_events(id, from, limit).await?),
        }
    }

    /// [`read`](Self::read), but with nothing at `from` yet in a live
    /// session, first wait up to `wait` for a record there or for the
    /// session to end (`session.read` with `wait`, protocol 6).
    ///
    /// [Decision Log]
    /// - Purpose: an agent waits for another node's session to finish a
    ///   turn without a subscription across hops (ADR 0013: a long poll,
    ///   as `fleet.watch`).
    /// - Choice: follow the session's ring from `from` and wait for its
    ///   first record there, or for the live stream to close (the session
    ///   ended); then read as usual, so the page is the same as without
    ///   waiting. Records already at `from`, an ended or stored session,
    ///   and a `from` the read refuses all answer at once.
    /// - Not the info watch: `next_seq` there moves with every record but
    ///   does not notify (that would wake every info watcher on every
    ///   event), so a wait on it slept through a turn's records until a
    ///   status changed or the wait ran out.
    /// - Bound: the caller caps `wait` ([`ReadEvents::MAX_WAIT`]), so a
    ///   waiting call holds one of the connection's call slots for at most
    ///   a minute.
    ///
    /// [`ReadEvents::MAX_WAIT`]: vessel_proto::ReadEvents::MAX_WAIT
    pub(crate) async fn read_waiting(
        &self,
        id: SessionId,
        from: EventSeq,
        limit: u32,
        wait: std::time::Duration,
    ) -> Result<EventPage, ServiceError> {
        if let Found::Live { handle, .. } = self.find(id).await?
            && let Ok(follow) = handle.follow(from)
            && follow.cached.is_empty()
        {
            // A record, a lag (records landed) or the end: read now.
            let mut live = follow.live;
            if tokio::time::timeout(wait, live.recv()).await.is_err() {
                tracing::trace!(session = %id, "a waiting read found nothing new");
            }
        }
        self.read(id, from, limit).await
    }
}

/// `page`, read from the store for a live session, unless retention pruned
/// records at its start that the session's ring still holds: then the page
/// comes from the ring, starting at `from` or at the ring's oldest record.
///
/// [Decision Log]
/// - Problem: a follower that lags catches up with `session.read`. When the
///   retention budget is smaller than the ring (`max_session_bytes` under
///   about 16 MiB with the default ring), the store can have dropped
///   records the ring still has. A store-only page then jumped over them,
///   and the follower reported them missing although the node had them. A
///   test waiting for output in that range waited forever.
/// - Alternatives: have the client resubscribe instead of reading (the
///   subscription path already prefers the ring), but paging exists so a
///   slow client catches up at its own pace without lagging the broadcast
///   again; or forbid budgets under the ring size, which would make
///   retention depend on a runtime setting.
/// - Choice: the read prefers the ring whenever the store's page does not
///   start at `from` and the ring reaches further back. Retention prunes a
///   prefix and the ring holds the newest records, so either source alone
///   is a gap-free suffix; no merge is needed.
/// - Cost: one ring lookup per read of a live session whose store page has
///   a gap, which only happens after pruning.
fn with_ring(page: EventPage, handle: &SessionHandle, from: EventSeq, limit: u32) -> EventPage {
    let stored_from = page.records.first().map(|record| record.seq);
    if stored_from == Some(from) {
        return page;
    }
    let follow = match handle.follow(from) {
        Ok(follow) => follow,
        Err(FollowError::Gap { oldest_cached }) => {
            // The ring starts later than `from`: it helps only if it starts
            // earlier than the store's page.
            if stored_from.is_some_and(|stored| stored <= oldest_cached) {
                return page;
            }
            match handle.follow(oldest_cached) {
                Ok(follow) => follow,
                // The ring moved on meanwhile; the store's page stands.
                Err(FollowError::Gap { .. } | FollowError::Ahead { .. }) => return page,
            }
        }
        Err(FollowError::Ahead { .. }) => return page,
    };
    let cached = follow.cached;
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let records: Vec<EventRecord> = cached
        .iter()
        .take(limit)
        .map(|record| EventRecord::clone(record))
        .collect();
    let Some(last) = records.last() else {
        return page;
    };
    EventPage {
        // `None` only for a log that is full; nothing follows its last
        // record, so asking for it again is as good an answer as any.
        next: last.seq.next().unwrap_or(last.seq),
        at_end: cached.len() <= limit,
        records,
    }
}

#[cfg(test)]
mod tests;
