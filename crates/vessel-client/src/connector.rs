//! [`Connector`]: how to open a connection to one node, again and again.
//!
//! [Decision Log]
//! - Purpose: a stream that survives a dropped connection has to dial the
//!   node again by itself, the same way the first connection was made. A
//!   websocket node needs a URL and a token; an SSH node (ADR 0007) needs an
//!   `ssh` process, the node's binary and its pinned id. The stream must not
//!   know which.
//! - Alternatives: a closure returning a future (no name for its error, no
//!   room for "is this worth retrying"); a boxed `dyn` trait (the futures
//!   would need boxing on every call, and nothing here needs a list of
//!   mixed connectors).
//! - Choice: a small trait with an associated error and an
//!   `impl Future` method (stable since Rust 1.75), implemented here for
//!   websockets ([`WsConnector`]) and in `vessel-remote` for SSH. It is
//!   generic, not `dyn`: each caller knows its connector's type.
//! - [`Connector::is_transient`] decides whether a failed dial is retried.
//!   It defaults to "yes", and each implementation says which of its errors
//!   are refusals (a wrong token, a changed node id) that the next attempt
//!   would only meet again.

use std::future::Future;
use std::sync::Arc;

use vessel_proto::ClientInfo;
use vessel_rpc::ClientConfig;

use crate::client::NodeClient;
use crate::error::ConnectError;
use crate::token::BearerToken;
use crate::url::NodeUrl;

/// Opens connections to one node.
///
/// Every call opens a new connection and runs the protocol handshake;
/// callers that keep a connection call it again only after losing it.
pub trait Connector: Send + Sync {
    /// Why a connection could not be opened.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Open a connection, introducing this side as `client`.
    fn connect(
        &self,
        client: ClientInfo,
    ) -> impl Future<Output = Result<NodeClient, Self::Error>> + Send;

    /// Whether a later attempt may succeed where this one failed (the node
    /// is down, the network failed) rather than being refused again.
    /// Reconnecting streams give up on errors for which this is `false`.
    fn is_transient(&self, error: &Self::Error) -> bool {
        let _ = error;
        true
    }
}

impl<C: Connector> Connector for Arc<C> {
    type Error = C::Error;

    fn connect(
        &self,
        client: ClientInfo,
    ) -> impl Future<Output = Result<NodeClient, Self::Error>> + Send {
        C::connect(self, client)
    }

    fn is_transient(&self, error: &Self::Error) -> bool {
        C::is_transient(self, error)
    }
}

/// Connects to a node's `/rpc` websocket with its bearer token.
#[derive(Debug, Clone)]
pub struct WsConnector {
    url: NodeUrl,
    token: BearerToken,
    config: ClientConfig,
}

impl WsConnector {
    /// Connect to `url` with `token`, with default limits.
    #[must_use]
    pub fn new(url: NodeUrl, token: BearerToken) -> Self {
        Self {
            url,
            token,
            config: ClientConfig::default(),
        }
    }

    /// The same, with explicit limits.
    #[must_use]
    pub fn with_config(mut self, config: ClientConfig) -> Self {
        self.config = config;
        self
    }

    /// Where it connects.
    #[must_use]
    pub const fn url(&self) -> &NodeUrl {
        &self.url
    }
}

impl Connector for WsConnector {
    type Error = ConnectError;

    fn connect(
        &self,
        client: ClientInfo,
    ) -> impl Future<Output = Result<NodeClient, ConnectError>> + Send {
        NodeClient::connect_with(&self.url, &self.token, client, &self.config)
    }

    fn is_transient(&self, error: &ConnectError) -> bool {
        error.is_transient()
    }
}
