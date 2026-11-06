//! `vessel session copy|move` and `vessel fleet memory copy` (protocol 6,
//! plan F4): asking the node a conversation or a memory goes to to pull
//! it from where it is.
//!
//! [Decision Log]
//! - The target pulls (`session.import`, `memory.import`), so the command
//!   talks to the target node and names the source by node id; the source
//!   node is asked only for its id. With both on one node (another
//!   profile), that is one connection's worth of work twice.
//! - A move is an import that resumes the copy, then a kill of the
//!   session here (protocol 6: "`move` is `import` then `session.kill` on
//!   the source, done by the client"). The source is killed only once the
//!   copy has started, so a failed import leaves the session running; a
//!   move refused mid-turn says so, and the session goes on.
//! - `--to` and `--from` are resolved as `--node` is (the node book,
//!   addresses), without `--token`, which belongs to `--node`'s node.

use std::fmt::Write as _;
use std::path::PathBuf;

use vessel_proto::{ImportMemory, ImportSession, MemoryRef, NodeId, ProfileName, SessionRef};

use crate::args::{CopyTo, Target};
use crate::connect::{Connected, connect};
use crate::error::CliError;
use crate::{io, render};

/// `target` with `node` in place of its node, and no token.
fn other(target: &Target, node: &str) -> Target {
    Target {
        node: Some(node.to_owned()),
        token: None,
        data_dir: target.data_dir.clone(),
        config_dir: target.config_dir.clone(),
    }
}

/// The node `node` names (`target`'s own without one), connected.
async fn node_or(target: &Target, node: Option<&str>) -> Result<Connected, CliError> {
    match node {
        None => connect(target).await,
        Some(node) => connect(&other(target, node)).await,
    }
}

/// `vessel session copy` (`start` to resume it there) and `vessel session
/// move` (`start` and `kill`).
pub(crate) async fn copy_session(
    target: &Target,
    to: CopyTo,
    start: bool,
    kill: bool,
) -> Result<(), CliError> {
    let CopyTo {
        session,
        to,
        profile,
        cwd,
    } = to;
    let source = connect(target).await?;
    let info = session.resolve(&source.client).await?;
    let from = SessionRef {
        node: source.client.node().id,
        session: info.id,
    };
    let destination = match &to {
        None => None,
        Some(node) => Some(connect(&other(target, node)).await?),
    };
    let there = destination.as_ref().unwrap_or(&source);
    let imported = there
        .client
        .import_session(ImportSession {
            from,
            profile,
            cwd,
            start,
        })
        .await?;
    let mut text = format!(
        "{} copied to {} under {} in {} ({} files written)\n",
        info.id,
        there.client.node().name,
        render::profile(imported.profile.as_ref()),
        imported.cwd.display(),
        imported.written,
    );
    if let Some(copy) = &imported.session {
        let _infallible = writeln!(text, "resumed there as {}", copy.id);
    }
    if kill {
        source.client.kill(info.id).await?;
        let _infallible = writeln!(text, "{} stopped here", info.id);
    }
    Ok(io::out(text.as_bytes()).await?)
}

/// Where `vessel fleet memory copy` copies from.
#[derive(Debug)]
pub(crate) struct MemorySource {
    /// The node (as for `--node`); `--node`'s without one.
    pub(crate) node: Option<String>,
    pub(crate) profile: Option<ProfileName>,
    pub(crate) project: Option<PathBuf>,
}

/// `vessel fleet memory copy`.
pub(crate) async fn copy_memory(
    target: &Target,
    source: MemorySource,
    to: Option<String>,
    profile: Option<ProfileName>,
    project: Option<PathBuf>,
) -> Result<(), CliError> {
    let there = node_or(target, to.as_deref()).await?;
    let node: NodeId = match &source.node {
        None if to.is_none() => there.client.node().id,
        None => connect(target).await?.client.node().id,
        Some(node) => connect(&other(target, node)).await?.client.node().id,
    };
    let imported = there
        .client
        .import_memory(ImportMemory {
            from: MemoryRef {
                node,
                profile: source.profile,
                project: source.project,
            },
            profile,
            project,
        })
        .await?;
    let mut text = format!(
        "{} memory files written, {} already there, {} beside a different file\n",
        imported.written,
        imported.same,
        imported.beside.len()
    );
    if !imported.beside.is_empty() {
        text.push_str("written as <name>.vessel-incoming beside the file there:\n");
    }
    for path in &imported.beside {
        let _infallible = writeln!(text, "  {path}");
    }
    Ok(io::out(text.as_bytes()).await?)
}
