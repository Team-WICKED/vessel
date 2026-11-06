//! Starting a node and stopping it.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_proto::{NodeId, NodeInfo};
use vessel_rpc::Server;
use vessel_session::{RuntimeConfig, SessionRuntime};
use vessel_store::{DataDir, NodeLock, NodeOwner, NodeToken, Opened, Store};

use crate::config::Config;
use crate::error::NodeError;
use crate::identity;
use crate::ipc::{self, Endpoint, LocalGateway, LocalListener};
use crate::lifecycle::Lifecycle;
use crate::server::{self, Gateway};
use crate::service::NodeService;
use crate::sessions::Sessions;
use crate::shutdown::Teardown;

/// A running node. Stop it with [`Node::stop`]; dropping it leaves it
/// running until the process exits.
#[derive(Debug)]
pub struct Node {
    address: Option<SocketAddr>,
    local: Endpoint,
    lifecycle: Lifecycle,
    stop: CancellationToken,
    task: JoinHandle<Result<(), NodeError>>,
}

/// What the blocking part of start-up produces.
struct Prepared {
    id: NodeId,
    token: NodeToken,
    lock: NodeLock,
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
        let dir = config.data_dir.clone();
        let Prepared { id, token, lock } = tokio::task::spawn_blocking(move || prepare(&dir))
            .await
            .map_err(NodeError::Task)??;
        let Opened { store, interrupted } = Store::open(&config.data_dir, config.store)
            .await
            .map_err(NodeError::Store)?;
        if !interrupted.is_empty() {
            tracing::info!(
                sessions = interrupted.len(),
                "sessions from the previous run are now interrupted"
            );
        }

        let runtime = Arc::new(SessionRuntime::new(
            RuntimeConfig {
                harness_lookup: config.harness_lookup.clone(),
                ring_capacity: config.ring_capacity,
                durable_capacity: config.durable_capacity,
                agent_handshake_timeout: config.agent_handshake_timeout,
            },
            CancellationToken::new(),
        ));
        let stop = CancellationToken::new();
        let lifecycle = Lifecycle::new(
            identity::node_info(id, config.name.clone()),
            stop.clone(),
            Arc::clone(&runtime),
        );
        let tasks = TaskTracker::new();
        let sessions = Arc::new(Sessions::new(
            Arc::clone(&runtime),
            store.clone(),
            tasks.clone(),
        ));
        let forwarders = TaskTracker::new();
        let service = NodeService::new(
            lifecycle.clone(),
            config.harness_lookup.clone(),
            sessions,
            forwarders.clone(),
        );
        let closing = CancellationToken::new();
        let rpc = Arc::new(Server::new(
            Arc::new(service),
            lifecycle.watch_info(),
            config.rpc.clone(),
            closing.clone(),
        ));
        let token = Arc::new(token);
        let connections = TaskTracker::new();

        let (local, tcp) = bind(&config, id).await?;
        let local_endpoint = local.endpoint().clone();
        let address = tcp.as_ref().map(|(_, address)| *address);

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
                    connections: connections.clone(),
                }),
            )
        });
        let gateway = LocalGateway {
            rpc,
            token,
            connections,
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

/// The blocking part of start-up: directory, identity, token, lock.
fn prepare(dir: &DataDir) -> Result<Prepared, NodeError> {
    dir.prepare().map_err(NodeError::DataDir)?;
    let id = dir.load_or_create_node_id().map_err(NodeError::DataDir)?;
    let token = dir.load_or_create_token().map_err(NodeError::DataDir)?;
    let owner = NodeOwner {
        pid: std::process::id(),
        executable: std::env::current_exe().unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at: Utc::now(),
    };
    let lock = NodeLock::acquire(dir, &owner).map_err(NodeError::AlreadyRunning)?;
    Ok(Prepared { id, token, lock })
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
