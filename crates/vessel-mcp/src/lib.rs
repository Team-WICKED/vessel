//! `vessel mcp`: the MCP server (stdio) a node attaches to every session as
//! `vessel` (ADR 0013), so the session's agent can list nodes and sessions,
//! start sessions, send them input, read and wait for their output, and
//! stop them.
//!
//! It reads where its node is and its session's credential from its own
//! environment ([`AgentEnv`], set by the node in the server's definition),
//! connects to the node's local socket as that session's agent, and turns
//! each tool call into one protocol call through that principal. It holds
//! no authority of its own: the node decides what the agent may do.
//!
//! [Decision Log: the MCP implementation] (plan decision 8)
//! - Rule: use `rmcp`, the official Rust SDK, if it passes AGENTS.md
//!   section 9; otherwise write the small server the plan sketches.
//! - The check (2026-11-06, rmcp 3.9.0): no `unsafe` of ours (rmcp's one
//!   `unsafe` block is in its own tests); the same tokio runtime, no other;
//!   features `server` and `transport-io` only, without the proc-macros,
//!   HTTP, auth or child-process transports. It adds nine crates to the
//!   tree (`rmcp`, `schemars` with its derive and two helpers,
//!   `dyn-clone`, `ref-cast` and its derive, `pastey`, `futures-executor`);
//!   the rest is already in the workspace. Passed: `rmcp` is used.
//! - The tools are written out by hand ([`VesselTools`] implements
//!   `ServerHandler`), with JSON Schemas as plain JSON: the `#[tool]` macros
//!   would generate code our lints check and could not be held to them.
//! - Protocol errors (an unknown tool, bad arguments) are JSON-RPC errors;
//!   a refusal by the node (`forbidden`, `conflict`) is a tool result marked
//!   as an error, with the node's code and message, so the agent can read
//!   why and go on.

mod env;
mod tools;

pub use env::{AgentEnv, EnvError, SECRET_VARIABLE, SESSION_VARIABLE, SOCKET_VARIABLE};
pub use tools::VesselTools;

use rmcp::ServiceExt as _;
use rmcp::service::ServerInitializeError;
use tokio::io::{AsyncRead, AsyncWrite};
use vessel_client::{LocalConnectError, LocalCredential, NodeClient};
use vessel_proto::ClientInfo;

/// Why `vessel mcp` stopped.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The environment does not say where the node is or who this is.
    #[error(transparent)]
    Env(#[from] EnvError),
    /// The node refused or could not be reached.
    #[error("cannot reach the node as this session's agent")]
    Connect(#[source] LocalConnectError),
    /// The MCP client did not complete `initialize`.
    #[error("the MCP client did not initialise")]
    Initialize(#[source] Box<ServerInitializeError>),
    /// The server task failed.
    #[error("the MCP server stopped unexpectedly")]
    Task(#[source] tokio::task::JoinError),
}

/// Connect to the node as the session's agent named by `env`.
///
/// # Errors
/// [`McpError::Connect`]: no node at the endpoint, or the credential is
/// refused (its session has ended, or it is not one).
pub async fn connect(env: &AgentEnv) -> Result<NodeClient, McpError> {
    let client = ClientInfo {
        name: "vessel-mcp".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    NodeClient::connect_local(
        &env.socket,
        &LocalCredential::Agent(env.auth.clone()),
        client,
    )
    .await
    .map_err(McpError::Connect)
}

/// Serve the tools on `read`/`write` until the MCP client leaves.
///
/// # Errors
/// The client did not initialise, or the server task failed.
pub async fn serve<R, W>(tools: VesselTools, read: R, write: W) -> Result<(), McpError>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let running = tools
        .serve((read, write))
        .await
        .map_err(|error| McpError::Initialize(Box::new(error)))?;
    let reason = running.waiting().await.map_err(McpError::Task)?;
    tracing::debug!(?reason, "the MCP client left");
    Ok(())
}

/// `vessel mcp`: read the environment, connect, and serve on stdio.
///
/// # Errors
/// See [`McpError`].
pub async fn run_stdio() -> Result<(), McpError> {
    let env = AgentEnv::from_env()?;
    let client = connect(&env).await?;
    let (read, write) = rmcp::transport::stdio();
    serve(VesselTools::new(client), read, write).await
}
