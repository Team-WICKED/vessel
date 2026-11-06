//! Which node a command talks to: [`resolve`] over the layers.

use std::path::{Path, PathBuf};

use vessel_proto::NodeId;

use super::{BookEntry, BookError, NodeBook, NodeName, ProjectConfig, TokenSource};
use crate::address::NodeAddress;
use crate::credential::CredentialSource;
use crate::url::NodeUrl;

/// The node a command talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Its name, if it came from the book.
    pub name: Option<NodeName>,
    /// How to reach it.
    pub address: NodeAddress,
    /// The book's token source for it.
    pub token: Option<TokenSource>,
    /// The book's device credential for it (paired nodes).
    pub credential: Option<CredentialSource>,
    /// The id the book pinned for it.
    pub pinned: Option<NodeId>,
    /// Which layer chose it.
    pub origin: Origin,
}

/// Which layer chose the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// `--node` or `VESSEL_NODE`.
    Named,
    /// The project file at this path.
    Project(PathBuf),
    /// The user book's `default_node`.
    User,
    /// Nothing chose: the node on this machine.
    Default,
}

/// The node to talk to: `named` (`--node`, or else `VESSEL_NODE`) if given,
/// else the project's `default_node`, else the book's, else this machine's
/// node at `local`.
///
/// `named` is looked up in the book first, then read as an address
/// ([`NodeAddress`]'s forms: `ssh://`, `user@host`, websocket URLs).
///
/// # Errors
/// `named` is neither a book name nor an address; the project's
/// `default_node` is not in the book.
pub fn resolve(
    book: &NodeBook,
    book_path: &Path,
    project: Option<&ProjectConfig>,
    named: Option<&str>,
    local: &NodeUrl,
) -> Result<Resolved, BookError> {
    if let Some(text) = named {
        if let Some((name, entry)) = book.get(text) {
            return Ok(from_entry(name, entry, Origin::Named));
        }
        let address = text.parse().map_err(|source| BookError::Unknown {
            text: text.to_owned(),
            source,
        })?;
        return Ok(Resolved {
            name: None,
            address,
            token: None,
            credential: None,
            pinned: None,
            origin: Origin::Named,
        });
    }
    if let Some(project) = project
        && let Some(name) = &project.default_node
    {
        let Some((name, entry)) = book.get(name.as_str()) else {
            return Err(BookError::UnknownDefault {
                path: project.path.clone(),
                name: name.to_string(),
                book: book_path.to_owned(),
            });
        };
        return Ok(from_entry(
            name,
            entry,
            Origin::Project(project.path.clone()),
        ));
    }
    if let Some(name) = &book.default_node
        && let Some((name, entry)) = book.get(name.as_str())
    {
        return Ok(from_entry(name, entry, Origin::User));
    }
    Ok(Resolved {
        name: None,
        address: NodeAddress::Ws(local.clone()),
        token: None,
        credential: None,
        pinned: None,
        origin: Origin::Default,
    })
}

fn from_entry(name: &NodeName, entry: &BookEntry, origin: Origin) -> Resolved {
    Resolved {
        name: Some(name.clone()),
        address: entry.address.clone(),
        token: entry.token.clone(),
        credential: entry.credential.clone(),
        pinned: entry.id,
        origin,
    }
}
