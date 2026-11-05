//! Vessel node daemon.
//!
//! One node runs per machine. It discovers the agent harnesses installed
//! there (Claude Code, Codex, ...), launches them on request, and streams
//! their output to any client over HTTP and websockets. Clients are the
//! `vessel` CLI today and editors tomorrow.

pub mod error;
pub mod harness;
pub mod routes;
pub mod session;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use vessel_core::{DEFAULT_PORT, NodeId, NodeInfo, PROTOCOL_VERSION};

pub use error::NodeError;
use routes::AppState;
use session::SessionManager;

/// How to run a node.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to listen on. Defaults to loopback only.
    pub bind: SocketAddr,
    /// Node name shown to clients. Defaults to the hostname.
    pub name: Option<String>,
    /// Where the node keeps its identity and, later, synced state.
    /// Defaults to `~/.vessel`.
    pub data_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)),
            name: None,
            data_dir: None,
        }
    }
}

fn hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn resolve_data_dir(config: &Config) -> PathBuf {
    config.data_dir.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".vessel")
    })
}

/// Load the node id from `<data_dir>/node.id`, creating one on first run.
fn load_or_create_id(data_dir: &PathBuf) -> Result<NodeId, NodeError> {
    std::fs::create_dir_all(data_dir).map_err(|e| NodeError::DataDir(data_dir.clone(), e))?;
    let path = data_dir.join("node.id");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(id) = text.parse::<NodeId>()
    {
        return Ok(id);
    }
    let id = NodeId::random();
    std::fs::write(&path, id.to_string()).map_err(|e| NodeError::DataDir(path, e))?;
    Ok(id)
}

/// Build the node's self-description.
///
/// # Errors
/// Fails if the data directory cannot be created or written.
pub fn node_info(config: &Config) -> Result<NodeInfo, NodeError> {
    let data_dir = resolve_data_dir(config);
    let id = load_or_create_id(&data_dir)?;
    let hostname = hostname();
    Ok(NodeInfo {
        id,
        name: config.name.clone().unwrap_or_else(|| hostname.clone()),
        hostname,
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol: PROTOCOL_VERSION,
        started_at: Utc::now(),
    })
}

/// Run the node until `Ctrl-C`.
///
/// # Errors
/// Fails if the data directory is unusable, the address cannot be bound,
/// or the server stops abnormally.
pub async fn serve(config: Config) -> Result<(), NodeError> {
    let node = node_info(&config)?;
    tracing::info!(id = %node.id, name = %node.name, bind = %config.bind, "vessel node starting");

    let state = Arc::new(AppState {
        node,
        sessions: SessionManager::new(),
    });
    let app = routes::router(state);

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| NodeError::Bind(config.bind, e))?;
    tracing::info!(bind = %config.bind, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(NodeError::Serve)
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::warn!(%err, "failed to listen for ctrl-c; running until killed");
        std::future::pending::<()>().await;
    }
    tracing::info!("shutdown requested");
}
