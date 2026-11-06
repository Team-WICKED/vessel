//! Starting a node and stopping it.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_proto::NodeInfo;
use vessel_rpc::Server;
use vessel_session::{RuntimeConfig, SessionRuntime};
use vessel_store::{Opened, Store};

use crate::config::Config;
use crate::error::NodeError;
use crate::identity;
use crate::server::{self, Gateway};
use crate::service::NodeService;
use crate::sessions::Sessions;
use crate::shutdown::Teardown;

/// A running node. Stop it with [`Node::stop`]; dropping it leaves it
/// running until the process exits.
#[derive(Debug)]
pub struct Node {
    address: SocketAddr,
    info: NodeInfo,
    stop: CancellationToken,
    task: JoinHandle<Result<(), NodeError>>,
}

impl Node {
    /// Prepare the data directory, open the event log (closing sessions a
    /// previous run left live as `Interrupted`), bind the listen address,
    /// and start serving. Returns once the node accepts connections.
    ///
    /// # Errors
    /// The data directory or event log is unusable, or the address cannot be
    /// bound.
    pub async fn start(config: Config) -> Result<Self, NodeError> {
        let dir = config.data_dir.clone();
        let (id, token) = tokio::task::spawn_blocking(move || {
            dir.prepare()?;
            Ok((dir.load_or_create_node_id()?, dir.load_or_create_token()?))
        })
        .await
        .map_err(NodeError::Task)?
        .map_err(NodeError::DataDir)?;
        let Opened { store, interrupted } = Store::open(&config.data_dir, config.store)
            .await
            .map_err(NodeError::Store)?;
        if !interrupted.is_empty() {
            tracing::info!(
                sessions = interrupted.len(),
                "sessions from the previous run are now interrupted"
            );
        }

        let info = identity::node_info(id, config.name.clone());
        let runtime = Arc::new(SessionRuntime::new(
            RuntimeConfig {
                harness_lookup: config.harness_lookup.clone(),
                ring_capacity: config.ring_capacity,
                durable_capacity: config.durable_capacity,
            },
            CancellationToken::new(),
        ));
        let tasks = TaskTracker::new();
        let sessions = Arc::new(Sessions::new(
            Arc::clone(&runtime),
            store.clone(),
            tasks.clone(),
        ));
        let forwarders = TaskTracker::new();
        let service = NodeService::new(
            info.clone(),
            config.harness_lookup.clone(),
            sessions,
            forwarders.clone(),
        );
        let closing = CancellationToken::new();
        let rpc = Arc::new(Server::new(
            Arc::new(service),
            tokio::sync::watch::channel(info.clone()).1,
            config.rpc.clone(),
            closing.clone(),
        ));

        let listener = TcpListener::bind(config.bind)
            .await
            .map_err(|source| NodeError::Bind {
                address: config.bind,
                source,
            })?;
        let address = listener.local_addr().map_err(|source| NodeError::Bind {
            address: config.bind,
            source,
        })?;
        if !address.ip().is_loopback() {
            tracing::warn!(
                %address,
                token = %config.data_dir.token_path().display(),
                "listening beyond loopback: anyone who can reach this address and \
                 read the token file can drive this node's harnesses"
            );
        }

        let connections = TaskTracker::new();
        let router = server::router(Gateway {
            rpc,
            token: Arc::new(token),
            connections: connections.clone(),
        });
        let teardown = Teardown {
            runtime,
            tasks,
            forwarders,
            closing,
            connections,
            store,
        };
        let stop = CancellationToken::new();
        let task = tokio::spawn(serve(listener, router, stop.clone(), teardown));
        tracing::info!(node = %info.id, name = %info.name, %address, "node listening");
        Ok(Self {
            address,
            info,
            stop,
            task,
        })
    }

    /// The address the node listens on (with the real port when the config
    /// asked for port 0).
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// The node's description.
    #[must_use]
    pub const fn info(&self) -> &NodeInfo {
        &self.info
    }

    /// Stop accepting, end every session as `Interrupted`, commit their
    /// logs, close connections and the store; returns when all of that is
    /// done.
    ///
    /// # Errors
    /// The HTTP server had failed.
    pub async fn stop(self) -> Result<(), NodeError> {
        self.stop.cancel();
        self.task.await.map_err(NodeError::Task)?
    }
}

async fn serve(
    listener: TcpListener,
    router: axum::Router,
    stop: CancellationToken,
    teardown: Teardown,
) -> Result<(), NodeError> {
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(stop.cancelled_owned())
        .await;
    teardown.run().await;
    served.map_err(NodeError::Serve)
}
