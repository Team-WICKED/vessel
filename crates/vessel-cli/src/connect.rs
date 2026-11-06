//! Connecting to the node named on the command line.

use vessel_client::{BearerToken, NodeClient};
use vessel_node::DataDir;
use vessel_proto::ClientInfo;

use crate::args::Target;
use crate::error::CliError;

/// The local data directory: `--data-dir`, or `~/.vessel`.
pub(crate) fn data_dir(target: &Target) -> Result<DataDir, CliError> {
    target
        .data_dir
        .clone()
        .or_else(DataDir::default_root)
        .map(DataDir::new)
        .ok_or(CliError::NoHome)
}

/// Connect with `--token`/`VESSEL_TOKEN`, or, for a node on this machine,
/// the local token file.
///
/// [Decision Log]
/// - Purpose: `vessel` on the node's own machine should just work, reading
///   the token the node wrote; talking to another machine must not leak it.
/// - Constraint: milestone 1 has no TLS (ADR 0005), so a token sent to a
///   remote `ws://` node crosses the network in clear text. The local token
///   file is this machine's node's secret; sending it to whatever host
///   `--node` names would hand it to that host and to the network.
/// - Choice: the token file is used only when `--node` is a loopback
///   address or `localhost` (decided from the text, no DNS). For any other
///   node the token must be given explicitly, and the error says so.
/// - Cost: reaching a local node through a non-loopback address of this
///   machine (its LAN IP) needs `--token` too.
pub(crate) async fn connect(target: &Target) -> Result<NodeClient, CliError> {
    let token = match &target.token {
        Some(token) => token.clone(),
        None if target.node.is_loopback() => local_token(data_dir(target)?).await?,
        None => return Err(CliError::TokenRequired(target.node.clone())),
    };
    let client = ClientInfo {
        name: "vessel-cli".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    Ok(NodeClient::connect(&target.node, &token, client).await?)
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
