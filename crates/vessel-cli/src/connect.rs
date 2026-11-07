//! Connecting to the node the command line names.
//!
//! A command first resolves its node ([`nodes::resolve`]: `--node`, the
//! project's default, the book's, or this machine's), then connects
//! through a [`NodeConnector`]. The connector is kept with the connection
//! ([`Connected`]) so that an attach can dial again the same way when the
//! connection drops (`vessel_client::ResumingStream`), and every dial
//! accepts only the node pinned in the book (`vessel_client::Pinned`).

use std::sync::Arc;

use vessel_client::book::{Resolved, TokenSource};
use vessel_client::secure::{DialError, ReachError, SecureConnector, WsDialer};
use vessel_client::{
    BearerToken, ConnectError, Connector, DeviceCredential, NodeAddress, NodeClient, NodeUrl,
    Pinned, PinnedError, SecureTarget, WsConnector,
};
use vessel_node::DataDir;
use vessel_proto::ClientInfo;

use vessel_remote::{SshConnectError, SshConnector};

use crate::args::Target;
use crate::error::CliError;
use crate::{nodes, remote};

/// The local data directory: `--data-dir`, or `~/.vessel`.
pub(crate) fn data_dir(target: &Target) -> Result<DataDir, CliError> {
    target
        .data_dir
        .clone()
        .or_else(DataDir::default_root)
        .map(DataDir::new)
        .ok_or(CliError::NoHome)
}

/// How this client introduces itself.
pub(crate) fn client_info() -> ClientInfo {
    ClientInfo {
        name: "vessel-cli".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// An open connection, and how to open another to the same node.
#[derive(Debug, Clone)]
pub(crate) struct Connected {
    pub(crate) client: NodeClient,
    pub(crate) connector: Arc<NodeConnector>,
}

/// Opens connections to the resolved node, whatever its transport.
#[derive(Debug)]
pub(crate) enum NodeConnector {
    Ws(Pinned<WsConnector>),
    /// Boxed: an SSH connector is a few hundred bytes (its `ssh` command
    /// line, data dir, provisioner), several times a websocket one.
    Ssh(Box<Pinned<SshConnector>>),
    /// A paired node over its routes (boxed for the same reason).
    Secure(Box<Pinned<SecureConnector<WsDialer>>>),
}

/// Why [`NodeConnector`] could not connect.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NodeConnectError {
    #[error(transparent)]
    Ws(PinnedError<ConnectError>),
    #[error(transparent)]
    Ssh(PinnedError<SshConnectError>),
    #[error(transparent)]
    Secure(PinnedError<ReachError<DialError>>),
}

impl Connector for NodeConnector {
    type Error = NodeConnectError;

    async fn connect(&self, client: ClientInfo) -> Result<NodeClient, NodeConnectError> {
        match self {
            Self::Ws(connector) => connector
                .connect(client)
                .await
                .map_err(NodeConnectError::Ws),
            Self::Ssh(connector) => connector
                .connect(client)
                .await
                .map_err(NodeConnectError::Ssh),
            Self::Secure(connector) => connector
                .connect(client)
                .await
                .map_err(NodeConnectError::Secure),
        }
    }

    fn is_transient(&self, error: &NodeConnectError) -> bool {
        match (self, error) {
            (Self::Ws(connector), NodeConnectError::Ws(error)) => connector.is_transient(error),
            (Self::Ssh(connector), NodeConnectError::Ssh(error)) => connector.is_transient(error),
            (Self::Secure(connector), NodeConnectError::Secure(error)) => {
                connector.is_transient(error)
            }
            // A connector only returns its own errors.
            (Self::Ws(_), NodeConnectError::Ssh(_) | NodeConnectError::Secure(_))
            | (Self::Ssh(_), NodeConnectError::Ws(_) | NodeConnectError::Secure(_))
            | (Self::Secure(_), NodeConnectError::Ws(_) | NodeConnectError::Ssh(_)) => false,
        }
    }
}

/// `error` as the CLI reports it; `book` is named when a pinned id did
/// not match.
fn report(error: NodeConnectError, book: &std::path::Path) -> CliError {
    let changed = |expected, found| CliError::Changed {
        expected,
        found,
        book: book.display().to_string(),
    };
    match error {
        NodeConnectError::Ws(PinnedError::Connect(error)) => CliError::Connect(error),
        NodeConnectError::Ssh(PinnedError::Connect(error)) => CliError::Remote(error),
        NodeConnectError::Secure(PinnedError::Connect(error)) => CliError::Reach(error),
        NodeConnectError::Ws(PinnedError::Changed { expected, found })
        | NodeConnectError::Ssh(PinnedError::Changed { expected, found })
        | NodeConnectError::Secure(PinnedError::Changed { expected, found }) => {
            changed(expected, found)
        }
    }
}

/// Resolve the node and connect to it.
pub(crate) async fn connect(target: &Target) -> Result<Connected, CliError> {
    let (resolved, book) = nodes::resolve(target)?;
    connect_to(target, &resolved, &book).await
}

/// Connect to `resolved`. A book node seen for the first time gets its id
/// recorded in the book at `book`.
pub(crate) async fn connect_to(
    target: &Target,
    resolved: &Resolved,
    book: &std::path::Path,
) -> Result<Connected, CliError> {
    let connector = match &resolved.address {
        NodeAddress::Ws(url) => {
            let token = token(target, resolved, url).await?;
            NodeConnector::Ws(Pinned::new(
                WsConnector::new(url.clone(), token),
                resolved.pinned,
            ))
        }
        NodeAddress::Ssh(node) => NodeConnector::Ssh(Box::new(Pinned::new(
            remote::connector(target, node).await?,
            resolved.pinned,
        ))),
        NodeAddress::Secure(node) => NodeConnector::Secure(Box::new(secure(resolved, node).await?)),
    };
    let client = connector
        .connect(client_info())
        .await
        .map_err(|error| report(error, book))?;
    if let Some(name) = &resolved.name
        && resolved.pinned.is_none()
    {
        nodes::pin(book, name, client.node().id).await;
    }
    Ok(Connected {
        client,
        connector: Arc::new(connector),
    })
}

/// The connector for a paired node: its credential file, bound to the
/// node's key and id (the book's pinned id, or else the credential's own),
/// checked before anything is dialled.
async fn secure(
    resolved: &Resolved,
    node: &SecureTarget,
) -> Result<Pinned<SecureConnector<WsDialer>>, CliError> {
    let source = resolved
        .credential
        .as_ref()
        .ok_or_else(|| CliError::NoCredential {
            node: nodes::label(resolved),
        })?;
    let path = source.path(dirs::home_dir().as_deref());
    let credential = tokio::task::spawn_blocking(move || DeviceCredential::read(&path))
        .await?
        .map_err(CliError::Credential)?;
    let id = resolved.pinned.unwrap_or(credential.node);
    let dialer = WsDialer::for_routes(node.routes()).map_err(CliError::Tls)?;
    let connector = SecureConnector::new(dialer, node.clone(), id, credential)?;
    Ok(Pinned::new(connector, Some(id)))
}

/// Whether `resolved` is the node on this machine: a loopback websocket
/// address, decided from the text (see `NodeUrl::is_loopback`).
pub(crate) fn is_here(resolved: &Resolved) -> bool {
    matches!(&resolved.address, NodeAddress::Ws(url) if url.is_loopback())
}

/// The token for a websocket node: `--token`/`VESSEL_TOKEN`, else the book
/// entry's token file, else, for a node on this machine, the local token
/// file.
///
/// [Decision Log]
/// - Purpose: `vessel` on the node's own machine should just work, reading
///   the token the node wrote; talking to another machine must not leak it.
/// - Constraint: milestone 1 has no TLS (ADR 0005), so a token sent to a
///   remote `ws://` node crosses the network in clear text. The local token
///   file is this machine's node's secret; sending it to whatever host
///   `--node` names would hand it to that host and to the network.
/// - Choice: the local token file is used only when the node's address is
///   a loopback address or `localhost` (decided from the text, no DNS). A
///   book entry may name a token file of its own (the user wrote it there
///   for that node). For any other node the token must be given
///   explicitly, and the error says so.
/// - Cost: reaching a local node through a non-loopback address of this
///   machine (its LAN IP) needs `--token` too.
async fn token(
    target: &Target,
    resolved: &Resolved,
    url: &NodeUrl,
) -> Result<BearerToken, CliError> {
    if let Some(token) = &target.token {
        return Ok(token.clone());
    }
    if let Some(source) = &resolved.token {
        return token_file(source).await;
    }
    if url.is_loopback() {
        return local_token(data_dir(target)?).await;
    }
    Err(CliError::TokenRequired(url.clone()))
}

async fn token_file(source: &TokenSource) -> Result<BearerToken, CliError> {
    let path = source.path(dirs::home_dir().as_deref());
    let read = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await?
    };
    let text = read.map_err(|source| CliError::TokenFile {
        path: path.clone(),
        source,
    })?;
    text.trim()
        .parse()
        .map_err(|_invalid| CliError::UnusableToken(path))
}

async fn local_token(dir: DataDir) -> Result<BearerToken, CliError> {
    let path = dir.token_path();
    let read = tokio::task::spawn_blocking(move || dir.read_token()).await?;
    let token = read.map_err(|source| CliError::Token {
        path: path.clone(),
        source,
    })?;
    // A node token is base64url text, so this only fails if the two token
    // types ever disagree.
    token
        .expose()
        .parse()
        .map_err(|_invalid| CliError::UnusableToken(path))
}
