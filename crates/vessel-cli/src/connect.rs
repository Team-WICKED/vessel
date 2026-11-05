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

/// Connect with `--token`/`VESSEL_TOKEN`, or the local token file.
pub(crate) async fn connect(target: &Target) -> Result<NodeClient, CliError> {
    let token = match &target.token {
        Some(token) => token.clone(),
        None => local_token(data_dir(target)?).await?,
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
