//! Starting a node and stopping it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_harness::HarnessEnv;
use vessel_proto::{NodeId, NodeInfo, NodePublicKey, RelayToken, RouteUrl};
use vessel_relay::{LinkState, NodeLink, TlsConfig, WsCarrier};
use vessel_rpc::Server;
use vessel_secure::NodeKeypair;
use vessel_session::{RuntimeConfig, SessionRuntime};
use vessel_store::{DataDir, NodeLock, NodeOwner, NodeToken, Opened, Store};

use crate::agents::Agents;
use crate::config::Config;
use crate::devices::{Devices, Routes};
use crate::error::NodeError;
use crate::fleet::{self, FleetFiles, FleetMcp};
use crate::guard::Guarded;
use crate::identity;
use crate::ipc::{self, AgentGateway, Endpoint, LocalGateway, LocalListener};
use crate::lifecycle::Lifecycle;
use crate::profiles::NodeProfiles;
use crate::profiles::launch::{AgentTools, Launcher, McpSource};
use crate::secure::{MAX_UNADMITTED, SecureGateway};
use crate::server::{self, Gateway};
use crate::service::{NodeService, Parts};
use crate::sessions::Sessions;
use crate::shutdown::Teardown;
use crate::transfers::{self, Transfers};

/// A running node. Stop it with [`Node::stop`]; dropping it leaves it
/// running until the process exits.
#[derive(Debug)]
pub struct Node {
    address: Option<SocketAddr>,
    local: Endpoint,
    lifecycle: Lifecycle,
    /// The gateway `/secure` and the relay links serve; tests reach it
    /// directly with in-memory carriers.
    #[cfg(test)]
    pub(crate) secure: SecureGateway,
    /// Each relay link's state.
    relays: Vec<(RouteUrl, watch::Receiver<LinkState>)>,
    stop: CancellationToken,
    task: JoinHandle<Result<(), NodeError>>,
}

/// What the blocking part of start-up produces.
struct Prepared {
    id: NodeId,
    token: NodeToken,
    /// From `node.key` (ADR 0009).
    keypair: NodeKeypair,
    lock: NodeLock,
    /// From `profiles.toml` (ADR 0011).
    profiles: NodeProfiles,
    /// Owner-only, emptied of a previous run's files (ADR 0012).
    mcp_dir: PathBuf,
}

impl Node {
    /// Prepare the data directory, take its node lock, open the event log
    /// (closing sessions a previous run left live as `Interrupted`), bind
    /// the local socket and the listen address (if any), and start
    /// serving. Returns once the node accepts connections.
    ///
    /// # Errors
    /// The data directory or event log is unusable, another node holds the
    /// data directory, or the local socket or the address cannot be bound.
    pub async fn start(config: Config) -> Result<Self, NodeError> {
        // Checked before anything starts, so a misconfigured relay is an
        // error at once rather than a node that is unreachable later.
        let relaying = relaying(&config)?;
        let dir = config.data_dir.clone();
        let env = config.harness_env.clone();
        let Prepared {
            id,
            token,
            keypair,
            lock,
            profiles,
            mcp_dir,
        } = tokio::task::spawn_blocking(move || prepare(&dir, env))
            .await
            .map_err(NodeError::Task)??;
        let store = open_store(&config).await?;
        let session_setup = SessionSetup::new(&config, id, profiles, mcp_dir);

        let Core {
            runtime,
            lifecycle,
            stop,
            tasks,
            forwarders,
            devices,
            agents,
            service,
        } = core(&config, id, &store, keypair.public(), session_setup);
        let closing = CancellationToken::new();
        let rpc = Arc::new(Server::new(
            Arc::clone(&service),
            lifecycle.watch_info(),
            config.rpc.clone(),
            closing.clone(),
        ));
        let token = Arc::new(token);
        let connections = TaskTracker::new();

        let (local, tcp) = bind(&config, id).await?;
        let local_endpoint = local.endpoint().clone();
        let address = tcp.as_ref().map(|(_, address)| *address);

        let secure = SecureGateway {
            keypair: Arc::new(keypair),
            channel: vessel_secure::Config::default(),
            service,
            lifecycle: lifecycle.clone(),
            devices,
            config: config.rpc.clone(),
            closing: closing.clone(),
            connections: connections.clone(),
            unadmitted: Arc::new(Semaphore::new(MAX_UNADMITTED)),
        };
        let teardown = Teardown {
            runtime,
            tasks,
            forwarders,
            closing,
            connections: connections.clone(),
            store,
        };
        let router = tcp.map(|(listener, _)| {
            (
                listener,
                server::router(Gateway {
                    rpc: Arc::clone(&rpc),
                    token: Arc::clone(&token),
                    secure: secure.clone(),
                    keepalive: config.keepalive,
                    connections: connections.clone(),
                }),
            )
        });
        let relays = link_relays(relaying, &config.relays, &secure, &connections, &stop);
        let gateway = LocalGateway {
            rpc,
            token,
            connections,
            agents: AgentGateway::new(agents, &secure, config.rpc.clone()),
        };
        let task = tokio::spawn(serve(
            router,
            (local, gateway),
            stop.clone(),
            teardown,
            lock,
        ));
        tracing::info!(
            node = %id,
            address = ?address,
            local = %local_endpoint.path().display(),
            "node listening"
        );
        Ok(Self {
            address,
            local: local_endpoint,
            lifecycle,
            #[cfg(test)]
            secure,
            relays,
            stop,
            task,
        })
    }

    /// The TCP address the node listens on (with the real port when the
    /// config asked for port 0); `None` when it listens on its local socket
    /// only.
    #[must_use]
    pub const fn address(&self) -> Option<SocketAddr> {
        self.address
    }

    /// The node's local socket (Unix) or named pipe (Windows), which
    /// `vesseld attach` connects to.
    #[must_use]
    pub fn local_endpoint(&self) -> &Path {
        self.local.path()
    }

    /// Each configured relay and where the node's link to it stands
    /// (connecting, registered, refused), to read or wait on.
    #[must_use]
    pub fn relay_links(&self) -> Vec<(RouteUrl, watch::Receiver<LinkState>)> {
        self.relays.clone()
    }

    /// The node's current description.
    #[must_use]
    pub fn info(&self) -> NodeInfo {
        self.lifecycle.info()
    }

    /// Resolves once the node was asked to stop, by [`Node::stop`] or by a
    /// client (`node.stop`, protocol 3). The caller then calls
    /// [`Node::stop`] to wait for the shutdown to finish.
    pub fn stop_requested(&self) -> impl Future<Output = ()> + Send + 'static {
        self.stop.clone().cancelled_owned()
    }

    /// Stop accepting, end every session as `Interrupted`, commit their
    /// logs, close connections and the store, and release the data
    /// directory; returns when all of that is done.
    ///
    /// # Errors
    /// The HTTP server had failed.
    pub async fn stop(self) -> Result<(), NodeError> {
        self.stop.cancel();
        self.task.await.map_err(NodeError::Task)?
    }
}

/// Open the event log, closing sessions a previous run left live.
async fn open_store(config: &Config) -> Result<Store, NodeError> {
    let Opened { store, interrupted } = Store::open(&config.data_dir, config.store)
        .await
        .map_err(NodeError::Store)?;
    if !interrupted.is_empty() {
        tracing::info!(
            sessions = interrupted.len(),
            "sessions from the previous run are now interrupted"
        );
    }
    Ok(store)
}

/// The node's parts below its transports.
struct Core {
    runtime: Arc<SessionRuntime>,
    lifecycle: Lifecycle,
    /// Cancelled to stop the node.
    stop: CancellationToken,
    /// Session creations and persist tasks.
    tasks: TaskTracker,
    /// Subscription forwarders.
    forwarders: TaskTracker,
    devices: Arc<Devices>,
    /// Sessions' agents (ADR 0013), admitted on the local socket.
    agents: Arc<Agents>,
    /// The service every transport serves, behind the scope check.
    service: Arc<Guarded>,
}

/// What sessions are created with: the node's profiles and the launch seam.
struct SessionSetup {
    profiles: Arc<NodeProfiles>,
    launcher: Launcher,
    /// The fleet's MCP servers, which the fleet loop keeps current and the
    /// launcher asks for.
    fleet_mcp: Arc<FleetMcp>,
}

impl SessionSetup {
    fn new(config: &Config, id: NodeId, profiles: NodeProfiles, mcp_dir: PathBuf) -> Self {
        let tools = config.vessel_mcp.clone().map(|mcp| AgentTools {
            program: mcp.program,
            args: mcp.args,
            socket: Endpoint::of(&config.data_dir, id).path().to_owned(),
        });
        let fleet_mcp = Arc::new(FleetMcp::default());
        let source: Arc<dyn McpSource> = Arc::<FleetMcp>::clone(&fleet_mcp);
        Self {
            profiles: Arc::new(profiles),
            launcher: Launcher::new(mcp_dir, source, tools),
            fleet_mcp,
        }
    }
}

/// Build the session runtime, the lifecycle, the devices and the service.
fn core(
    config: &Config,
    id: NodeId,
    store: &Store,
    key: &NodePublicKey,
    setup: SessionSetup,
) -> Core {
    let runtime = Arc::new(SessionRuntime::new(
        RuntimeConfig {
            harness_lookup: config.harness_lookup.clone(),
            harness_env: config.harness_env.clone(),
            ring_capacity: config.ring_capacity,
            durable_capacity: config.durable_capacity,
            agent_handshake_timeout: config.agent_handshake_timeout,
        },
        CancellationToken::new(),
    ));
    let stop = CancellationToken::new();
    let lifecycle = Lifecycle::new(
        identity::node_info(id, config.name.clone(), *key),
        stop.clone(),
        Arc::clone(&runtime),
    );
    let tasks = TaskTracker::new();
    let agents = Arc::new(Agents::new(id));
    let sessions = Arc::new(Sessions::new(
        Arc::clone(&runtime),
        store.clone(),
        tasks.clone(),
        Arc::clone(&agents),
    ));
    let forwarders = TaskTracker::new();
    let devices = Arc::new(Devices::new(
        store.clone(),
        id,
        *key,
        Routes {
            relays: config.relays.clone(),
            direct: config.public.clone(),
        },
    ));
    // Fleet sync's tasks are session-adjacent work: shutdown waits for an
    // apply in progress before the data directory is released.
    let links = Arc::new(fleet::Links::new(
        id,
        lifecycle.info().name,
        FleetFiles::new(&config.data_dir),
        Arc::clone(&devices),
    ));
    let fleet = fleet::start(
        FleetFiles::new(&config.data_dir),
        &config.fleet,
        &setup.profiles,
        setup.fleet_mcp,
        Arc::clone(&links),
        &tasks,
        &stop,
    );
    let transfers = Arc::new(Transfers::new(
        transfers::root(&config.data_dir),
        transfers::IDLE,
    ));
    transfers.sweep(&tasks, &stop);
    let service = Arc::new(Guarded(NodeService::new(Parts {
        lifecycle: lifecycle.clone(),
        lookup: config.harness_lookup.clone(),
        sessions,
        devices: Arc::clone(&devices),
        forwarders: forwarders.clone(),
        profiles: setup.profiles,
        launcher: setup.launcher,
        agents: Arc::clone(&agents),
        fleet,
        links,
        transfers,
    })));
    Core {
        runtime,
        lifecycle,
        stop,
        tasks,
        forwarders,
        devices,
        agents,
        service,
    }
}

/// Bind the local socket, and the TCP address if the config has one.
async fn bind(
    config: &Config,
    id: NodeId,
) -> Result<(LocalListener, Option<(TcpListener, SocketAddr)>), NodeError> {
    let endpoint = Endpoint::of(&config.data_dir, id);
    let path = endpoint.path().to_path_buf();
    let local =
        LocalListener::bind(endpoint).map_err(|source| NodeError::LocalSocket { path, source })?;
    let tcp = match config.bind {
        Some(bind) => Some(listen(bind, &config.data_dir).await?),
        None => None,
    };
    Ok((local, tcp))
}

/// The relays' registration token and the TLS setup for them, if any
/// relay is configured. The token file is read even without relays, so a
/// broken one shows before it is needed.
fn relaying(config: &Config) -> Result<Option<(RelayToken, TlsConfig)>, NodeError> {
    let token = match &config.relay_token_file {
        Some(path) => Some(vessel_relay::read_token_file(path)?),
        None => None,
    };
    if config.relays.is_empty() {
        return Ok(None);
    }
    let token = token.ok_or(NodeError::NoRelayToken)?;
    Ok(Some((token, relay_tls(&config.relays)?)))
}

/// TLS for `wss://` relays: the system's root certificates (plan, decision
/// 1). Only a `wss://` relay needs them, so a machine without any can
/// still use `ws://` relays.
fn relay_tls(relays: &[RouteUrl]) -> Result<TlsConfig, NodeError> {
    match TlsConfig::native_roots() {
        Ok(tls) => Ok(tls),
        Err(error)
            if relays
                .iter()
                .any(|relay| relay.as_str().starts_with("wss://")) =>
        {
            Err(NodeError::RelayTls(error))
        }
        Err(error) => {
            tracing::debug!(%error, "no TLS roots; only ws:// relays are configured");
            TlsConfig::with_roots(rustls::RootCertStore::empty()).map_err(NodeError::RelayTls)
        }
    }
}

/// One [`NodeLink`] per relay, until the node stops. Each client a relay
/// splices to the node arrives as a carrier and takes the same path as a
/// direct `/secure` connection.
///
/// [Decision Log]
/// - Lifetime: the links stop with `stop` (the node was asked to stop), so
///   no new client arrives while the node shuts down; the connections they
///   delivered close with the others. Link tasks and their connections
///   are tracked with the node's connections, so shutdown waits for them.
/// - Alternatives: one task registering with every relay (a slow relay
///   would hold the others back); links started lazily on the first
///   pairing (a node that was paired before a restart would be
///   unreachable until then).
fn link_relays(
    relaying: Option<(RelayToken, TlsConfig)>,
    relays: &[RouteUrl],
    gateway: &SecureGateway,
    connections: &TaskTracker,
    stop: &CancellationToken,
) -> Vec<(RouteUrl, watch::Receiver<LinkState>)> {
    let Some((token, tls)) = relaying else {
        return Vec::new();
    };
    let mut states = Vec::with_capacity(relays.len());
    for relay in relays {
        let link = NodeLink::new(
            relay.clone(),
            Arc::clone(&gateway.keypair),
            token.clone(),
            tls.clone(),
        );
        let gateway = gateway.clone();
        let tracker = connections.clone();
        let accept = move |carrier: WsCarrier| {
            let gateway = gateway.clone();
            drop(tracker.spawn(async move {
                gateway.accept_logged(carrier, "relay").await;
            }));
        };
        states.push((relay.clone(), link.state()));
        tracing::info!(%relay, "linking to a relay");
        drop(connections.spawn(link.run(accept, stop.clone())));
    }
    states
}

/// The blocking part of start-up: directory, identity, token, key, lock.
fn prepare(dir: &DataDir, env: HarnessEnv) -> Result<Prepared, NodeError> {
    dir.prepare().map_err(NodeError::DataDir)?;
    let id = dir.load_or_create_node_id().map_err(NodeError::DataDir)?;
    let token = dir.load_or_create_token().map_err(NodeError::DataDir)?;
    // Created on the first start of every node, new or existing (ADR 0009),
    // so its identity exists before any device pairs.
    let key = dir.load_or_create_node_key().map_err(NodeError::DataDir)?;
    let keypair = NodeKeypair::from_secret(*key.expose());
    let owner = NodeOwner {
        pid: std::process::id(),
        executable: std::env::current_exe().unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at: Utc::now(),
    };
    let lock = NodeLock::acquire(dir, &owner).map_err(NodeError::AlreadyRunning)?;
    // After the lock: the MCP directory is emptied, which only the node
    // owning the data directory may do.
    let mcp_dir = dir.prepare_mcp_dir().map_err(NodeError::McpDir)?;
    // Transfers expire at restart (protocol 6); same reason to wait for the
    // lock.
    transfers::clear(dir);
    let profiles = NodeProfiles::load(dir.clone(), env).map_err(NodeError::Profiles)?;
    Ok(Prepared {
        id,
        token,
        keypair,
        lock,
        profiles,
        mcp_dir,
    })
}

/// Bind the TCP listen address.
async fn listen(bind: SocketAddr, dir: &DataDir) -> Result<(TcpListener, SocketAddr), NodeError> {
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| NodeError::Bind {
            address: bind,
            source,
        })?;
    let address = listener.local_addr().map_err(|source| NodeError::Bind {
        address: bind,
        source,
    })?;
    if !address.ip().is_loopback() {
        tracing::warn!(
            %address,
            token = %dir.token_path().display(),
            "listening beyond loopback: anyone who can reach this address and \
             read the token file can drive this node's harnesses"
        );
    }
    Ok((listener, address))
}

/// Serve the local socket and (if bound) HTTP until `stop`, then shut down
/// in order and release the data directory last.
async fn serve(
    tcp: Option<(TcpListener, axum::Router)>,
    (local, gateway): (LocalListener, LocalGateway),
    stop: CancellationToken,
    teardown: Teardown,
    lock: NodeLock,
) -> Result<(), NodeError> {
    let http = async {
        if let Some((listener, router)) = tcp {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.clone().cancelled_owned())
                .await
                .map_err(NodeError::Serve)
        } else {
            stop.cancelled().await;
            Ok(())
        }
    };
    let ((), served) = tokio::join!(ipc::serve(local, gateway, stop.clone()), http);
    teardown.run().await;
    drop(lock);
    served
}
