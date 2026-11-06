//! The node book on the command line: which node `--node` means, and
//! `vessel nodes add|list|show|remove|default`.
//!
//! The book itself (layering, the project file's limits, edits that keep
//! comments) is `vessel_client::book`; this module finds its files and
//! prints it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use vessel_client::book::{
    BookEntry, BookError, BookFile, NodeBook, NodeName, ProjectConfig, Resolved, TokenSource,
};
use vessel_client::{NodeAddress, NodeUrl, RemotePath};
use vessel_proto::DEFAULT_PORT;

use crate::args::{NodesCommand, Target};
use crate::error::CliError;
use crate::io;

/// The user's book file: `<config_dir>/vessel/nodes.toml`.
pub(crate) fn book_path(target: &Target) -> Result<PathBuf, CliError> {
    target
        .config_dir
        .clone()
        .or_else(dirs::config_dir)
        .map(|dir| NodeBook::path_in(&dir))
        .ok_or(CliError::NoHome)
}

/// The node on this machine, when nothing else is chosen.
fn local_node() -> Result<NodeUrl, CliError> {
    let text = format!("127.0.0.1:{DEFAULT_PORT}");
    text.parse().map_err(|source| CliError::Address {
        text,
        source: vessel_client::InvalidAddress::Ws(source),
    })
}

/// The node this command talks to (`--node`/`VESSEL_NODE`, the project's
/// default, the book's default, or this machine's), with its book entry's
/// details.
pub(crate) fn resolve(target: &Target) -> Result<(Resolved, PathBuf), CliError> {
    let path = book_path(target)?;
    let book = BookFile::open(&path)?;
    let project = match std::env::current_dir() {
        Ok(cwd) => ProjectConfig::find(&cwd, dirs::home_dir().as_deref())?,
        Err(_unknown) => None,
    };
    let resolved = vessel_client::book::resolve(
        book.book(),
        &path,
        project.as_ref(),
        target.node.as_deref(),
        &local_node()?,
    )?;
    tracing::debug!(node = %resolved.address, origin = ?resolved.origin, "node chosen");
    Ok((resolved, path))
}

/// How to refer to a resolved node in messages: its name, or its address.
pub(crate) fn label(resolved: &Resolved) -> String {
    resolved
        .name
        .as_ref()
        .map_or_else(|| resolved.address.to_string(), ToString::to_string)
}

/// Record the id seen on first contact with a book node. A failure to
/// write is reported but does not fail the command: the connection itself
/// is fine, and the next contact tries again.
pub(crate) async fn pin(book: &Path, name: &NodeName, id: vessel_proto::NodeId) {
    let path = book.to_owned();
    let name = name.clone();
    let pinned = tokio::task::spawn_blocking(move || {
        BookFile::open(&path).and_then(|mut file| file.pin(name.as_str(), id))
    })
    .await;
    match pinned {
        Ok(Ok(())) => tracing::debug!(%id, "pinned the node's id in the book"),
        Ok(Err(error)) => tracing::warn!(%error, "could not record the node's id in the book"),
        Err(error) => tracing::warn!(%error, "could not record the node's id in the book"),
    }
}

/// `vessel nodes ...`.
pub(crate) async fn command(target: &Target, command: NodesCommand) -> Result<(), CliError> {
    let path = book_path(target)?;
    let text = tokio::task::spawn_blocking(move || edit(&path, command)).await??;
    Ok(io::out(text.as_bytes()).await?)
}

/// Run one book command (blocking file I/O); returns what to print.
fn edit(path: &Path, command: NodesCommand) -> Result<String, CliError> {
    let mut file = BookFile::open(path)?;
    match command {
        NodesCommand::Add {
            name,
            address,
            token_file,
            remote_data_dir,
            default,
        } => {
            let entry = entry(&address, token_file, remote_data_dir)?;
            file.add(&name, &entry)?;
            if default {
                file.set_default(Some(name.as_str()))?;
            }
            Ok(format!(
                "added {name} ({}) to {}\n",
                entry.address,
                path.display()
            ))
        }
        NodesCommand::List => Ok(list(file.book())),
        NodesCommand::Show { name } => {
            let Some((name, entry)) = file.book().get(&name) else {
                return Err(BookError::Missing {
                    path: path.to_owned(),
                    name,
                }
                .into());
            };
            Ok(show(
                name,
                entry,
                file.book().default_node.as_ref() == Some(name),
            ))
        }
        NodesCommand::Remove { name } => {
            file.remove(&name)?;
            Ok(format!("removed {name}\n"))
        }
        NodesCommand::Default { name, none: _ } => {
            file.set_default(name.as_deref())?;
            Ok(match name {
                Some(name) => format!("default node: {name}\n"),
                None => "no default node; commands use the node on this machine\n".to_owned(),
            })
        }
    }
}

/// A book entry from `vessel nodes add`'s arguments.
fn entry(
    address: &str,
    token_file: Option<PathBuf>,
    remote_data_dir: Option<RemotePath>,
) -> Result<BookEntry, CliError> {
    let mut address: NodeAddress = address.parse().map_err(|source| CliError::Address {
        text: address.to_owned(),
        source,
    })?;
    match &mut address {
        NodeAddress::Ssh(target) => {
            if token_file.is_some() {
                return Err(CliError::Usage(
                    "--token-file is for websocket nodes; an SSH node needs no token",
                ));
            }
            target.data_dir = remote_data_dir;
        }
        NodeAddress::Ws(_) => {
            if remote_data_dir.is_some() {
                return Err(CliError::Usage(
                    "--remote-data-dir is for SSH nodes; a websocket node runs its own",
                ));
            }
        }
    }
    Ok(BookEntry {
        address,
        token: token_file.map(TokenSource::File),
        id: None,
    })
}

fn list(book: &NodeBook) -> String {
    if book.nodes.is_empty() {
        return "no nodes yet; add one with `vessel nodes add NAME [user@]host`\n".to_owned();
    }
    let width = book
        .nodes
        .keys()
        .map(|name| name.as_str().len())
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for (name, entry) in &book.nodes {
        let marker = if book.default_node.as_ref() == Some(name) {
            "*"
        } else {
            " "
        };
        let _infallible = writeln!(text, "{marker} {name:<width$}  {}", entry.address);
    }
    text
}

fn show(name: &NodeName, entry: &BookEntry, default: bool) -> String {
    let mut text = format!("name: {name}{}\n", if default { " (default)" } else { "" });
    match &entry.address {
        NodeAddress::Ssh(target) => {
            let _infallible = writeln!(text, "ssh: {target}");
            let dir = target
                .data_dir
                .clone()
                .unwrap_or_else(RemotePath::default_data_dir);
            let _infallible = writeln!(text, "data dir: {dir}");
        }
        NodeAddress::Ws(url) => {
            let _infallible = writeln!(text, "url: {url}");
            if let Some(TokenSource::File(file)) = &entry.token {
                let _infallible = writeln!(text, "token file: {}", file.display());
            }
        }
    }
    match entry.id {
        Some(id) => {
            let _infallible = writeln!(text, "id: {id}");
        }
        None => text.push_str("id: not seen yet (recorded on first contact)\n"),
    }
    text
}
