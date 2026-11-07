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
//!   (`vessel_llm::client`). A key that is not distinctive
//!   (`ApiKey::distinctive`: under 20 bytes, or under ten different ones)
//!   refuses the call, before anything goes up: every answer is searched
//!   for the key, and a model writes `EMPTY` or `sk-1234` as ordinary
//!   text. The check sits where the key is read, the only place its value
//!   is known (a secret may be set or changed after `models.toml` is
//!   loaded); a local server is given no key at all.
//! - Limits: a session makes at most [`PER_SESSION`] calls at once and the
//!   node at most [`PER_NODE`], so a leaked token or a looping agent cannot
//!   spend without bound in parallel; a call past them is refused as a rate
//!   limit, which harnesses back off from.

pub(crate) mod config;
mod listen;
pub(crate) mod proxy;
#[cfg(test)]
mod tests;
pub(crate) mod token;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use vessel_harness::{ModelProxy, Profile};
use vessel_llm::client::{ApiKey, Client};
use vessel_proto::{ModelRoute, ModelRouteName, ProfileInfo, SecretName, SessionId, SessionModel};
use vessel_session::SessionHandle;
use vessel_store::DataDir;

use crate::error::ServiceError;
use crate::fleet::FleetFiles;
pub use config::ModelsError;
use config::{Provider, Route, Routes};
use token::{Holder, ModelToken, MoveTokenError, Tokens};

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

    /// What a session of `profile` is launched with when the profile has
    /// a model route (ADR 0015): the route as recorded, and the proxy with
    /// a fresh token for session `id`, which opens nothing until the
    /// session has [`started`](Self::started). `None` for a profile
    /// without a route.
    ///
    /// # Errors
    /// The route is not in `models.toml`, or no random bytes.
    pub(crate) fn launch(
        &self,
        profile: Option<&Profile>,
        id: SessionId,
    ) -> Result<Option<(SessionModel, ModelProxy)>, ServiceError> {
        let Some(name) = profile.and_then(Profile::model) else {
            return Ok(None);
        };
        let (route, _provider) = self
            .route(name)
            .ok_or_else(|| missing_route(profile, name))?;
        let token = self
            .tokens
            .mint(id, name.clone())
            .map_err(ServiceError::Random)?;
        let proxy = self.proxy(&route, &token)?;
        Ok(Some((
            SessionModel {
                route: name.clone(),
                model: route.model,
            },
            proxy,
        )))
    }

    /// A profile move of session `id`, now on `current` (its recorded
    /// route), to `target` begins: the proxy and a second token for the
    /// agent the move starts, when the session is routed. A session keeps
    /// its route: moving it between profiles of different routes (or a
    /// route and none) is refused, since its record and its harness's
    /// threads (Codex files them under the provider) belong to the route.
    ///
    /// # Errors
    /// Different routes, a route no longer in `models.toml`, another move
    /// running, or no random bytes.
    pub(crate) fn begin_move(
        &self,
        id: SessionId,
        current: Option<&ModelRouteName>,
        target: &Profile,
    ) -> Result<Option<ModelProxy>, ServiceError> {
        if current != target.model() {
            return Err(ServiceError::MoveRoute {
                from: current.cloned(),
                to: target.model().cloned(),
            });
        }
        let Some(name) = current else {
            return Ok(None);
        };
        let (route, _provider) = self
            .route(name)
            .ok_or_else(|| missing_route(Some(target), name))?;
        let token = match self.tokens.begin_move(id) {
            Ok(Some(token)) => token,
            // A live routed session has its token from launch.
            Ok(None) => return Err(ServiceError::Unrouted(id)),
            Err(MoveTokenError::Running) => return Err(ServiceError::MoveRunning(id)),
            Err(MoveTokenError::Random(error)) => return Err(ServiceError::Random(error)),
        };
        self.proxy(&route, &token).map(Some)
    }

    /// The move begun with [`begin_move`](Self::begin_move) ended.
    pub(crate) fn end_move(&self, id: SessionId, moved: bool) {
        self.tokens.end_move(id, moved);
    }

    fn proxy(&self, route: &Route, token: &ModelToken) -> Result<ModelProxy, ServiceError> {
        Ok(ModelProxy {
            base_url: self.base_url().ok_or(ServiceError::ModelProxyDown)?,
            token: token.to_string(),
            model: route.model.clone(),
            small_model: route.small_model.clone(),
        })
    }

    /// Warn about profiles whose route `models.toml` does not have: their
    /// sessions are refused until it does.
    pub(crate) fn check_profiles(&self, profiles: &[ProfileInfo]) {
        let routes = self.snapshot();
        for profile in profiles {
            if let Some(name) = &profile.model
                && routes.get(name).is_none()
            {
                tracing::warn!(
                    profile = %profile.name,
                    route = %name,
                    "a profile names a model route models.toml does not have; its sessions are refused"
                );
            }
        }
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
        let key = value
            .map(|value| ApiKey::new(value.expose().to_owned()))
            .ok_or_else(|| missing_key(&name))?;
        // Every answer is searched for the key (`proxy`): one a model may
        // write as ordinary text would end valid answers.
        if !key.distinctive() {
            tracing::warn!(secret = %name, "refused a model call: the route's key is too plain");
            return Err(plain_key(&name));
        }
        Ok(Some(key))
    }

    /// The upstream client.
    pub(crate) const fn client(&self) -> &Client {
        &self.client
    }

    /// The proxy's base URL for harnesses (`http://127.0.0.1:<port>`), once
    /// it listens.
    pub(crate) fn base_url(&self) -> Option<String> {
        self.addr.get().map(|addr| format!("http://{addr}"))
    }

    /// The proxy's port, once it listens (its `Host` check).
    pub(crate) fn port(&self) -> Option<u16> {
        self.addr.get().map(SocketAddr::port)
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
        Self::for_tests_with(dir, vessel_llm::client::Limits::default())
    }

    /// [`for_tests`](Self::for_tests) with the client's `limits`.
    #[cfg(test)]
    pub(crate) fn for_tests_with(dir: &DataDir, limits: vessel_llm::client::Limits) -> Self {
        let client = Client::with_roots(rustls::RootCertStore::empty(), limits).unwrap();
        Self::load(dir.clone(), FleetFiles::new(dir), client).unwrap()
    }
}

fn missing_route(profile: Option<&Profile>, name: &ModelRouteName) -> ServiceError {
    ServiceError::UnknownRoute {
        profile: profile.map(|profile| profile.name().clone()),
        route: name.clone(),
    }
}

fn plain_key(name: &SecretName) -> String {
    format!(
        "the route's provider key `{name}` is too short or too plain to keep out of model \
         answers: give the provider no key in models.toml (a local server needs none), \
         or a random one of at least {} characters",
        vessel_llm::client::MIN_KEY_BYTES
    )
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
