//! The node's sessions: live ones in the runtime, and every one ever
//! stored, including those from earlier runs.
//!
//! A session created in this run is "live" here for the whole run, even
//! after its process ended: the runtime still has its ring for followers
//! and the node still knows how far its log is committed. Sessions from
//! earlier runs exist only in the store; they can be listed, read and
//! followed, and refuse input with `Conflict`.
//!
//! The only state this type adds to the runtime's is each live session's
//! durability watch, kept in a small id-keyed table that is written once per
//! session and otherwise only read (lock held for a lookup, never across an
//! `.await`).

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use tokio::sync::watch;
use tokio_util::task::TaskTracker;
use vessel_proto::{EventPage, EventSeq, SessionId, SessionInfo, SessionInput, SessionSpec};
use vessel_session::{Created, SessionHandle, SessionRuntime};
use vessel_store::Store;

use crate::error::ServiceError;
use crate::persist::{self, Durability};

/// Where a session was found.
pub(crate) enum Found {
    /// Created in this run.
    Live {
        handle: SessionHandle,
        durability: watch::Receiver<Durability>,
    },
    /// From an earlier run (ended).
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
}

impl Sessions {
    pub(crate) fn new(runtime: Arc<SessionRuntime>, store: Store, tasks: TaskTracker) -> Self {
        Self {
            runtime,
            store,
            durability: RwLock::new(HashMap::new()),
            tasks,
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
    ) -> Result<SessionInfo, ServiceError> {
        let sessions = Arc::clone(self);
        self.tasks
            .spawn(async move { sessions.start(spec).await })
            .await
            .map_err(ServiceError::Task)?
    }

    async fn start(&self, spec: SessionSpec) -> Result<SessionInfo, ServiceError> {
        let Created { handle, durable } = self.runtime.create(spec).await?;
        if let Err(error) = self.store.create_session(handle.info()).await {
            // Without a stored row its records cannot be kept; stop it.
            // Dropping `durable` lets it finish without waiting for us.
            handle.kill();
            return Err(error.into());
        }
        let (durability, watching) = watch::channel(Durability::Through(EventSeq::ZERO));
        let previous = self
            .durability
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(handle.id(), watching);
        if previous.is_some() {
            tracing::error!(session = %handle.id(), "session id created twice");
        }
        let info = handle.info();
        drop(self.tasks.spawn(persist::run(
            self.store.clone(),
            handle,
            durable,
            durability,
        )));
        Ok(info)
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
    /// which is ahead of the store's.
    pub(crate) async fn list(&self) -> Result<Vec<SessionInfo>, ServiceError> {
        let mut all: HashMap<SessionId, SessionInfo> = self
            .store
            .sessions()
            .await?
            .into_iter()
            .map(|info| (info.id, info))
            .collect();
        for info in self.runtime.list() {
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
            Found::Stored(_) => Err(ServiceError::EndedEarlier(id)),
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
    /// lag.
    pub(crate) async fn read(
        &self,
        id: SessionId,
        from: EventSeq,
        limit: u32,
    ) -> Result<EventPage, ServiceError> {
        if let Found::Live {
            handle,
            mut durability,
        } = self.find(id).await?
        {
            let produced = handle.info().next_seq;
            persist::wait_until(id, &mut durability, produced).await?;
        }
        Ok(self.store.read_events(id, from, limit).await?)
    }
}
