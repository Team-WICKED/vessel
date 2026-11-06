//! The book's file format: what a file may say (every field optional,
//! unknown keys refused), checked into a [`NodeBook`], and a
//! [`BookEntry`] written back as a table.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use toml_edit::{Array, InlineTable, Table, value};
use vessel_proto::{NodeId, NodePublicKey, RouteUrl};

use super::{BookEntry, BookError, EntryProblem, NodeBook, NodeName, TokenSource};
use crate::address::{NodeAddress, RemotePath, SecureTarget, SshTarget};
use crate::credential::CredentialSource;
use crate::url::NodeUrl;

/// The user's book file as written: every field optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BookContent {
    default_node: Option<String>,
    #[serde(default)]
    nodes: BTreeMap<String, EntryContent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EntryContent {
    ssh: Option<String>,
    url: Option<String>,
    key: Option<String>,
    data_dir: Option<String>,
    token: Option<FileContent>,
    relay: Option<Vec<String>>,
    direct: Option<Vec<String>>,
    credential: Option<FileContent>,
    id: Option<String>,
}

/// `{ file = "..." }`: where a token or credential is kept.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileContent {
    file: PathBuf,
}

/// A project file as written: only `default_node`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectContent {
    pub(super) default_node: Option<String>,
}

/// The file's content as a checked book.
pub(super) fn check(path: &Path, content: BookContent) -> Result<NodeBook, BookError> {
    let mut nodes = BTreeMap::new();
    for (name, entry) in content.nodes {
        let problem = |problem| BookError::Entry {
            path: path.to_owned(),
            name: name.clone(),
            problem,
        };
        let key: NodeName = name
            .parse()
            .map_err(|invalid| problem(EntryProblem::Name(invalid)))?;
        let entry = check_entry(entry).map_err(problem)?;
        let _unique = nodes.insert(key, entry);
    }
    let default_node = match content.default_node {
        Some(name) => {
            let Some(key) = name.parse().ok().filter(|key| nodes.contains_key(key)) else {
                return Err(BookError::UnknownDefault {
                    path: path.to_owned(),
                    name,
                    book: path.to_owned(),
                });
            };
            Some(key)
        }
        None => None,
    };
    Ok(NodeBook {
        default_node,
        nodes,
    })
}

fn check_entry(entry: EntryContent) -> Result<BookEntry, EntryProblem> {
    let id = entry
        .id
        .map(|id| {
            id.parse::<NodeId>()
                .map_err(|_invalid| EntryProblem::Id(id))
        })
        .transpose()?;
    let secure_fields =
        entry.credential.is_some() || entry.relay.is_some() || entry.direct.is_some();
    let address = match (entry.ssh, entry.url, entry.key) {
        (None, None, None) => return Err(EntryProblem::NoTransport),
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
            return Err(EntryProblem::ManyTransports);
        }
        (Some(ssh), None, None) => {
            if secure_fields {
                return Err(EntryProblem::SecureFieldsWithoutKey);
            }
            if entry.token.is_some() {
                return Err(EntryProblem::TokenOverSsh);
            }
            let mut target: SshTarget = ssh.parse()?;
            target.data_dir = entry
                .data_dir
                .map(|dir| dir.parse::<RemotePath>())
                .transpose()?;
            NodeAddress::Ssh(target)
        }
        (None, Some(url), None) => {
            if secure_fields {
                return Err(EntryProblem::SecureFieldsWithoutKey);
            }
            if entry.data_dir.is_some() {
                return Err(EntryProblem::DataDirOverWs);
            }
            let url: NodeUrl = url.parse().map_err(|error: crate::url::InvalidNodeUrl| {
                EntryProblem::Url(error.to_string())
            })?;
            NodeAddress::Ws(url)
        }
        (None, None, Some(key)) => {
            if entry.token.is_some() || entry.data_dir.is_some() {
                return Err(EntryProblem::WsFieldsWithKey);
            }
            if entry.credential.is_none() {
                return Err(EntryProblem::NoCredential);
            }
            let target = SecureTarget {
                key: key
                    .parse::<NodePublicKey>()
                    .map_err(|error| EntryProblem::Key(error.to_string()))?,
                relays: routes(entry.relay)?,
                direct: routes(entry.direct)?,
            };
            if target.routes().next().is_none() {
                return Err(EntryProblem::NoRoute);
            }
            NodeAddress::Secure(target)
        }
    };
    Ok(BookEntry {
        address,
        token: entry.token.map(|token| TokenSource::File(token.file)),
        credential: entry
            .credential
            .map(|credential| CredentialSource::File(credential.file)),
        id,
    })
}

fn routes(texts: Option<Vec<String>>) -> Result<Vec<RouteUrl>, EntryProblem> {
    texts
        .unwrap_or_default()
        .iter()
        .map(|text| {
            text.parse()
                .map_err(|error: vessel_proto::InvalidRoute| EntryProblem::Route(error.to_string()))
        })
        .collect()
}

/// `entry` as a `[nodes.<name>]` table.
pub(super) fn entry_table(entry: &BookEntry) -> Result<Table, EntryProblem> {
    let mut table = Table::new();
    match &entry.address {
        NodeAddress::Ssh(target) => {
            if entry.token.is_some() {
                return Err(EntryProblem::TokenOverSsh);
            }
            let _new = table.insert("ssh", value(target.to_string()));
            if let Some(dir) = &target.data_dir {
                let _new = table.insert("data_dir", value(dir.as_str()));
            }
        }
        NodeAddress::Ws(url) => {
            let _new = table.insert("url", value(url.to_string()));
            if let Some(TokenSource::File(file)) = &entry.token {
                let file = file.to_str().ok_or(EntryProblem::TokenPath)?;
                let mut token = InlineTable::new();
                let _new = token.insert("file", file.into());
                let _new = table.insert("token", value(token));
            }
        }
        NodeAddress::Secure(target) => {
            if entry.token.is_some() {
                return Err(EntryProblem::WsFieldsWithKey);
            }
            let Some(CredentialSource::File(file)) = &entry.credential else {
                return Err(EntryProblem::NoCredential);
            };
            let _new = table.insert("key", value(target.key.to_string()));
            for (field, routes) in [("relay", &target.relays), ("direct", &target.direct)] {
                if !routes.is_empty() {
                    let list: Array = routes.iter().map(RouteUrl::as_str).collect();
                    let _new = table.insert(field, value(list));
                }
            }
            let file = file.to_str().ok_or(EntryProblem::CredentialPath)?;
            let mut credential = InlineTable::new();
            let _new = credential.insert("file", file.into());
            let _new = table.insert("credential", value(credential));
        }
    }
    if let Some(id) = entry.id {
        let _new = table.insert("id", value(id.to_string()));
    }
    Ok(table)
}
