//! Model routing (milestone 5, ADR 0015): the routes in `models.toml`, the
//! tokens routed sessions present, and the proxy on loopback that turns a
//! harness's model calls into calls to the route's provider.
//!
//! [Decision Log]
//! - Purpose: every harness can use every model. A harness speaks its own
//!   vendor's API to an endpoint it lets be changed; the node's proxy is
//!   that endpoint, and calls the route's provider in the provider's API
//!   (`vessel-llm` translates, or passes the bytes through when both sides
//!   speak the same API).
//! - State: the routes behind a `std` `RwLock`, swapped whole on reload and
//!   cloned out under the lock, as profiles are; a call uses the routes in
//!   effect when it arrived. Tokens and limits live here for the node's
//!   life; a restart voids them with the sessions it interrupts.
//! - Keys: a provider's key is read from `secrets.toml` for each call, in
//!   memory only, and goes nowhere but the upstream's authorization header
//!   (`vessel_llm::client`).
//! - Limits: a session makes at most [`PER_SESSION`] calls at once and the
//!   node at most [`PER_NODE`], so a leaked token or a looping agent cannot
//!   spend without bound in parallel; a call past them is refused as a rate
//!   limit, which harnesses back off from.

pub(crate) mod config;
pub(crate) mod proxy;
#[cfg(test)]
mod tests;
pub(crate) mod token;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use vessel_llm::client::{ApiKey, Client};
use vessel_proto::{ModelRoute, ModelRouteName, SecretName, SessionId};
use vessel_session::SessionHandle;
use vessel_store::DataDir;

use crate::fleet::FleetFiles;
pub use config::ModelsError;
use config::{Provider, Route, Routes};
use token::{Holder, ModelToken, Tokens};

/// Calls one session may have open at once.
pub(crate) const PER_SESSION: usize = 4;

/// Calls the node may have open at once.
pub(crate) const PER_NODE: usize = 16;

/// The largest request body the proxy reads.
pub(crate) const MAX_REQUEST: usize = 32 * 1024 * 1024;

/// The node's model routes, tokens and limits.
#[derive(Debug)]
pub(crate) struct Models {
    dir: DataDir,
    files: FleetFiles,
    current: RwLock<Arc<Routes>>,
    tokens: Tokens,
    client: Client,
    node_slots: Arc<Semaphore>,
    session_slots: Mutex<HashMap<SessionId, Arc<Semaphore>>>,
    /// Where the proxy listens, once it does.
    addr: OnceLock<SocketAddr>,
}

impl Models {
    /// Read `models.toml` (none means no routes). Blocks on the file
    /// system.
    ///
    /// # Errors
    /// The file cannot be read, other users can read it, or it does not
    /// parse.
    pub(crate) fn load(
        dir: DataDir,
        files: FleetFiles,
        client: Client,
    ) -> Result<Self, ModelsError> {
        let routes = read(&dir)?;
        log_loaded(&routes);
        Ok(Self {
            dir,
            files,
            current: RwLock::new(Arc::new(routes)),
            tokens: Tokens::default(),
            client,
            node_slots: Arc::new(Semaphore::new(PER_NODE)),
            session_slots: Mutex::new(HashMap::new()),
            addr: OnceLock::new(),
        })
    }

    /// Read the file again and use it; a file that cannot be used leaves
    /// the routes in effect. Blocks on the file system.
    ///
    /// # Errors
    /// As for [`load`](Self::load).
    pub(crate) fn reload(&self) -> Result<(), ModelsError> {
        let routes = read(&self.dir)?;
        log_loaded(&routes);
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(routes);
        Ok(())
    }

    /// The routes as clients see them (`model.list`).
    pub(crate) fn list(&self) -> Vec<ModelRoute> {
        self.snapshot().list()
    }

    /// The route named `name` and its provider, in the routes in effect.
    pub(crate) fn route(&self, name: &ModelRouteName) -> Option<(Route, Provider)> {
        self.snapshot()
            .get(name)
            .map(|(route, provider)| (route.clone(), provider.clone()))
    }

    fn snapshot(&self) -> Arc<Routes> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// A token for the calls of session `id`, about to be launched on
    /// `route`; it opens nothing until [`started`](Self::started).
    ///
    /// # Errors
    /// No random bytes.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "launching a routed session mints its token (M5 step B7)"
        )
    )]
    pub(crate) fn mint(
        &self,
        id: SessionId,
        route: ModelRouteName,
    ) -> Result<ModelToken, getrandom::Error> {
        self.tokens.mint(id, route)
    }

    /// Session `id` has started: its token, if it has one, opens its calls.
    pub(crate) fn started(&self, id: SessionId, handle: SessionHandle) {
        self.tokens.started(id, handle);
    }

    /// Session `id` has ended or did not start: its token and its call
    /// limit go.
    pub(crate) fn retire(&self, id: SessionId) {
        self.tokens.retire(id);
        let _slots = self
            .session_slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// The session and route a presented token belongs to.
    pub(crate) fn holder(&self, presented: &str) -> Option<Holder> {
        self.tokens.find(presented)
    }

    /// A place for one call of `session`, or `None` past the limits.
    pub(crate) fn slot(
        &self,
        session: SessionId,
    ) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let per_session = Arc::clone(
            self.session_slots
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(session)
                .or_insert_with(|| Arc::new(Semaphore::new(PER_SESSION))),
        );
        let session_permit = per_session.try_acquire_owned().ok()?;
        let node_permit = Arc::clone(&self.node_slots).try_acquire_owned().ok()?;
        Some((session_permit, node_permit))
    }

    /// The key of `provider`, read from `secrets.toml` now. `Ok(None)` for
    /// a provider that takes none.
    ///
    /// # Errors
    /// The provider names a secret that is not set, or the file cannot be
    /// read; the message names the secret, never a value.
    pub(crate) async fn key(&self, provider: &Provider) -> Result<Option<ApiKey>, String> {
        let Some(name) = provider.key.clone() else {
            return Ok(None);
        };
        let files = self.files.clone();
        let wanted = name.clone();
        let read = tokio::task::spawn_blocking(move || crate::fleet::secret_value(&files, &wanted))
            .await
            .map_err(|_panicked| "the node could not read its secrets".to_owned())?;
        let value = read.map_err(|error| {
            tracing::warn!(%error, "cannot read secrets.toml for a model call");
            "the node could not read its secrets".to_owned()
        })?;
        value
            .map(|value| Some(ApiKey::new(value.expose().to_owned())))
            .ok_or_else(|| missing_key(&name))
    }

    /// The upstream client.
    pub(crate) const fn client(&self) -> &Client {
        &self.client
    }

    /// The proxy's base URL for harnesses (`http://127.0.0.1:<port>`), once
    /// it listens.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "a routed session's harness is pointed here (M5 step B7)"
        )
    )]
    pub(crate) fn base_url(&self) -> Option<String> {
        self.addr.get().map(|addr| format!("http://{addr}"))
    }

    /// The proxy listens at `addr`.
    pub(crate) fn listening(&self, addr: SocketAddr) {
        let _first = self.addr.set(addr);
    }

    /// The models of a node keeping its state in `dir`, with an upstream
    /// client that trusts no certificate authority (tests call plain-HTTP
    /// fakes only).
    #[cfg(test)]
    pub(crate) fn for_tests(dir: &DataDir) -> Self {
        let client = Client::with_roots(
            rustls::RootCertStore::empty(),
            vessel_llm::client::Limits::default(),
        )
        .unwrap();
        Self::load(dir.clone(), FleetFiles::new(dir), client).unwrap()
    }
}

fn missing_key(name: &SecretName) -> String {
    format!(
        "the route's provider key `{name}` is not set on this node (`vessel secret set {name}`)"
    )
}

fn read(dir: &DataDir) -> Result<Routes, ModelsError> {
    let Some(file) = dir.read_models().map_err(ModelsError::Read)? else {
        return Ok(Routes::default());
    };
    if file.readable_by_others {
        return Err(ModelsError::NotOwnerOnly);
    }
    config::parse(&file.text)
}

fn log_loaded(routes: &Routes) {
    if routes.len() > 0 {
        tracing::info!(routes = routes.len(), "loaded models.toml");
    }
}
