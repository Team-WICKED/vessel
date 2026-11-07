//! The data directory and the node's identity: id, token, key. Created
//! once each (`vessel-store` creates them exclusively, so a node starting
//! at the same time is no problem); already there, they are reported and
//! left alone.

use super::{Init, Outcome, shown};
use crate::error::CliError;

pub(super) async fn run(init: &mut Init) -> Result<(), CliError> {
    let dir = init.data_dir.clone();
    let (existed, id) = tokio::task::spawn_blocking(move || {
        let existed = dir.read_node_id().ok().flatten().is_some()
            && dir.token_path().is_file()
            && dir.node_key_path().is_file();
        dir.prepare()?;
        let id = dir.load_or_create_node_id()?;
        let _token = dir.load_or_create_token()?;
        let _key = dir.load_or_create_node_key()?;
        Ok::<_, vessel_node::StoreError>((existed, id))
    })
    .await
    .map_err(|error| CliError::Io(std::io::Error::other(error)))?
    .map_err(CliError::Prepare)?;
    let outcome = if existed {
        Outcome::Already
    } else {
        Outcome::Done
    };
    let detail = format!("{} (node {id})", shown(init, init.data_dir.root()));
    init.report(outcome, "node identity", &detail).await?;
    Ok(())
}
