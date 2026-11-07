//! `vessel ui`'s nodes: the book's, and this machine's (ADR 0017).
//!
//! The bridge (`vessel_web`) names nodes and asks this source for a way to
//! reach each; everything that needs the CLI's knowledge stays here:
//! the book, token files, SSH, device credentials, pinned ids, pairing.
//!
//! [Decision Log]
//! - Only book names and this machine's node are reached. `vessel
//!   --node` also takes an address; the UI does not, so nothing a page
//!   sends can make this process dial a host (or start `ssh` to one) that
//!   the user did not put in the book.
//! - `--node` and `--token` (and `VESSEL_NODE`, `VESSEL_TOKEN`) are
//!   ignored: they name one node for one command, and a token given for
//!   one node must not be sent to every node the UI lists.
//! - The book is read again on every request, so a node added with `vessel
//!   nodes add` while the UI runs shows up without a restart.

use vessel_client::NodeAddress;
use vessel_client::book::{BookEntry, BookFile, NodeBook, NodeName, Origin, Resolved};
use vessel_proto::{NodeId, PairingLink};
use vessel_web::NodeSource;
use vessel_web::wire::{BookNode, NodeKey, Transport};

use crate::args::Target;
use crate::connect::{self, NodeConnector};
use crate::error::CliError;
use crate::{nodes, pair};

/// The book and this machine's node, for `vessel ui`.
#[derive(Debug)]
pub(crate) struct BookSource {
    /// The data and configuration directories only (see the module's
    /// notes).
    target: Target,
}

impl BookSource {
    pub(crate) fn new(target: &Target) -> Self {
        Self {
            target: Target {
                node: None,
                token: None,
                data_dir: target.data_dir.clone(),
                config_dir: target.config_dir.clone(),
            },
        }
    }

    async fn book(&self) -> Result<(NodeBook, std::path::PathBuf), CliError> {
        let path = nodes::book_path(&self.target)?;
        let reading = path.clone();
        let file = tokio::task::spawn_blocking(move || BookFile::open(&reading)).await??;
        Ok((file.book().clone(), path))
    }
}

impl NodeSource for BookSource {
    type Connector = NodeConnector;
    type Error = CliError;

    async fn nodes(&self) -> Result<Vec<BookNode>, CliError> {
        let (book, _path) = self.book().await?;
        let mut listed = vec![BookNode {
            key: NodeKey::here(),
            label: "this machine".to_owned(),
            address: nodes::local_node()?.to_string(),
            transport: Transport::Ws,
            default: book.default_node.is_none(),
            id: None,
        }];
        listed.extend(
            book.nodes
                .iter()
                .map(|(name, entry)| listed_node(name, entry, &book)),
        );
        Ok(listed)
    }

    async fn connector(&self, node: &NodeKey) -> Result<NodeConnector, CliError> {
        let resolved = if node.is_here() {
            Resolved {
                name: None,
                address: NodeAddress::Ws(nodes::local_node()?),
                token: None,
                credential: None,
                pinned: None,
                origin: Origin::Default,
            }
        } else {
            let (book, _path) = self.book().await?;
            let (name, entry) = book
                .get(node.as_str())
                .ok_or_else(|| CliError::NotInBook(node.to_string()))?;
            Resolved {
                name: Some(name.clone()),
                address: entry.address.clone(),
                token: entry.token.clone(),
                credential: entry.credential.clone(),
                pinned: entry.id,
                origin: Origin::Named,
            }
        };
        connect::connector(&self.target, &resolved).await
    }

    async fn seen(&self, node: &NodeKey, id: NodeId) {
        if node.is_here() {
            return;
        }
        // Pinned once, on first contact, as the CLI does.
        let Ok((book, path)) = self.book().await else {
            return;
        };
        if let Some((name, entry)) = book.get(node.as_str())
            && entry.id.is_none()
        {
            nodes::pin(&path, name, id).await;
        }
    }

    async fn pair(&self, name: &str, link: &str) -> Result<BookNode, CliError> {
        let name: NodeName = name.parse()?;
        let offer = link
            .trim()
            .parse::<PairingLink>()
            .and_then(|link| link.decode())
            .map_err(CliError::PairingLink)?;
        let _joined = pair::with_offer(&self.target, &name, &offer, false).await?;
        let (book, _path) = self.book().await?;
        let (name, entry) = book
            .get(name.as_str())
            .ok_or_else(|| CliError::NotInBook(name.to_string()))?;
        Ok(listed_node(name, entry, &book))
    }
}

/// A book entry as the UI lists it.
fn listed_node(name: &NodeName, entry: &BookEntry, book: &NodeBook) -> BookNode {
    let (address, transport) = match &entry.address {
        NodeAddress::Ws(url) => (url.to_string(), Transport::Ws),
        NodeAddress::Ssh(target) => (target.to_string(), Transport::Ssh),
        NodeAddress::Secure(target) => (
            format!("key {}", vessel_client::fingerprint(&target.key)),
            Transport::Paired,
        ),
    };
    BookNode {
        key: NodeKey::new(name.as_str()),
        label: name.to_string(),
        address,
        transport,
        default: book.default_node.as_ref() == Some(name),
        id: entry.id,
    }
}
