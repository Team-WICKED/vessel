//! Device credentials on the client (ADR 0010): what a paired device
//! presents to its node, one file per node.
//!
//! [Decision Log]
//! - Purpose: a paired device authenticates with its device id and a
//!   256-bit secret issued at pairing. It must survive restarts, and must
//!   not end up where the node book does (the book is edited by hand and
//!   may be shared between machines).
//! - Choice: `<config_dir>/vessel/credentials/<node id>.secret`, a small
//!   TOML file (`node`, `key`, `device`, `secret`), owner-only on Unix
//!   (directory 0700, file 0600). The book points to it (`credential = {
//!   file = ... }`). Named by node id, so pairing the same node again
//!   replaces it.
//! - Bound to its node: the file names the node's id and key from the
//!   pairing link, and nothing sends the secret to any other key or id
//!   ([`DeviceCredential::check`], before a dial). The book is shared and
//!   edited by hand, so an entry may name someone else's key and relay
//!   while pointing at this file; without the binding the handshake to
//!   that key would succeed and the secret would go to its holder.
//! - Written under a temporary name (random, so a stale file from a
//!   crashed run cannot collide) and renamed into place, so a crash
//!   leaves the old credential or the new one, never half of one.
//! - Windows: the files inherit the user profile's access control, which
//!   keeps other users out; no extra ACL code.
//! - Moving a device means pairing it again, not copying this file (ADR
//!   0010); it is bearer material, like the node's own key.

use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use vessel_proto::{DeviceId, DeviceSecret, NodeId, NodePublicKey};

/// Where a paired node's device credential is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// A credential file (`~/` is the local home directory).
    File(PathBuf),
}

impl CredentialSource {
    /// The file to read, with a leading `~/` expanded against `home`.
    #[must_use]
    pub fn path(&self, home: Option<&Path>) -> PathBuf {
        let Self::File(path) = self;
        match (path.strip_prefix("~"), home) {
            (Ok(rest), Some(home)) => home.join(rest),
            _ => path.clone(),
        }
    }
}

/// A device's credential for one node.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCredential {
    /// The node it was issued by (the pairing link's id).
    pub node: NodeId,
    /// That node's key (the pairing link's): the only key it is sent to.
    pub key: NodePublicKey,
    /// The device, as the node knows it.
    pub device: DeviceId,
    /// Its secret.
    pub secret: DeviceSecret,
}

impl fmt::Debug for DeviceCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCredential")
            .field("node", &self.node)
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

/// A credential file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// The file could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The file does not hold a credential.
    #[error("{} is not a device credential: {message}", path.display())]
    Invalid {
        /// Which file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
}

/// A credential that belongs to another node than the one it would be sent
/// to. Never sent; final (pair this node, or fix the book).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialMismatch {
    /// The node's key is not the credential's.
    #[error(
        "the credential is for the node with key {credential}, not {target}; it is not sent \
         (pair this node, or fix its entry in the book)"
    )]
    Key {
        /// The key the credential belongs to.
        credential: NodePublicKey,
        /// The key it would have gone to.
        target: NodePublicKey,
    },
    /// The node's id is not the credential's.
    #[error(
        "the credential is for node {credential}, not {pinned}; it is not sent (pair this \
         node, or fix its entry in the book)"
    )]
    Node {
        /// The node the credential belongs to.
        credential: NodeId,
        /// The node it would have gone to.
        pinned: NodeId,
    },
}

impl DeviceCredential {
    /// Whether this credential may be sent to the node with `key` and id
    /// `node`: only to the node that issued it.
    ///
    /// # Errors
    /// The key or the id is another node's.
    pub fn check(&self, key: &NodePublicKey, node: NodeId) -> Result<(), CredentialMismatch> {
        if self.key != *key {
            return Err(CredentialMismatch::Key {
                credential: self.key,
                target: *key,
            });
        }
        if self.node != node {
            return Err(CredentialMismatch::Node {
                credential: self.node,
                pinned: node,
            });
        }
        Ok(())
    }

    /// Where pairing puts the credential for `node`, under a configuration
    /// directory (`dirs::config_dir()` in the CLI).
    #[must_use]
    pub fn path_in(config_dir: &Path, node: NodeId) -> PathBuf {
        config_dir
            .join("vessel")
            .join("credentials")
            .join(format!("{node}.secret"))
    }

    /// Read a credential file.
    ///
    /// # Errors
    /// The file cannot be read or does not hold a credential.
    pub fn read(path: &Path) -> Result<Self, CredentialError> {
        let text = std::fs::read_to_string(path).map_err(|source| CredentialError::Io {
            path: path.to_owned(),
            source,
        })?;
        toml_edit::de::from_str(&text).map_err(|error| CredentialError::Invalid {
            path: path.to_owned(),
            message: error.message().to_owned(),
        })
    }

    /// Write this credential to `path`, replacing any older one: the
    /// directory owner-only (0700), the file 0600 (Unix).
    ///
    /// # Errors
    /// The directory or file cannot be created or written.
    pub fn write(&self, path: &Path) -> Result<(), CredentialError> {
        let io = |source| CredentialError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            private_dir(dir).map_err(io)?;
        }
        let temporary = temporary_name(path).map_err(io)?;
        let text = format!(
            "# A Vessel device credential. Keep it secret; to move this device,\n\
             # pair it again, and revoke it on the node if it is lost.\n\
             node = \"{}\"\nkey = \"{}\"\ndevice = \"{}\"\nsecret = \"{}\"\n",
            self.node, self.key, self.device, self.secret
        );
        let written = new_private_file(&temporary)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())
                    .and_then(|()| file.sync_all())
            })
            .and_then(|()| std::fs::rename(&temporary, path));
        if written.is_err() {
            drop(std::fs::remove_file(&temporary));
        }
        written.map_err(io)
    }
}

/// `.<name>.<random>.tmp` beside `path`.
fn temporary_name(path: &Path) -> io::Result<PathBuf> {
    let mut suffix = [0_u8; 8];
    getrandom::fill(&mut suffix).map_err(io::Error::other)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(path.with_file_name(format!(".{name}.{:016x}.tmp", u64::from_le_bytes(suffix))))
}

#[cfg(unix)]
fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn new_private_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn new_private_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_round_trips_owner_only_and_never_prints_its_secret() {
        let temp = tempfile::tempdir().unwrap();
        let node = NodeId::random();
        let path = DeviceCredential::path_in(temp.path(), node);
        let key = NodePublicKey::from_bytes([3; 32]);
        let credential = DeviceCredential {
            node,
            key,
            device: DeviceId::random(),
            secret: DeviceSecret::from_bytes([8; 32]),
        };
        credential.write(&path).unwrap();
        assert_eq!(DeviceCredential::read(&path).unwrap(), credential);
        assert!(!format!("{credential:?}").contains(&credential.secret.to_string()));

        let replaced = DeviceCredential {
            node,
            key,
            device: DeviceId::random(),
            secret: DeviceSecret::from_bytes([9; 32]),
        };
        replaced.write(&path).unwrap();
        assert_eq!(DeviceCredential::read(&path).unwrap(), replaced);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        std::fs::write(&path, "device = 1\n").unwrap();
        assert!(matches!(
            DeviceCredential::read(&path),
            Err(CredentialError::Invalid { .. })
        ));
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary file is left behind");
    }

    /// A book entry can name any key and relay while pointing at this
    /// file; the credential still only goes to the node that issued it.
    #[test]
    fn a_credential_is_only_for_the_node_that_issued_it() {
        let node = NodeId::random();
        let key = NodePublicKey::from_bytes([3; 32]);
        let credential = DeviceCredential {
            node,
            key,
            device: DeviceId::random(),
            secret: DeviceSecret::from_bytes([8; 32]),
        };
        assert!(credential.check(&key, node).is_ok());
        let attacker = NodePublicKey::from_bytes([4; 32]);
        assert!(matches!(
            credential.check(&attacker, node),
            Err(CredentialMismatch::Key { .. })
        ));
        assert!(matches!(
            credential.check(&key, NodeId::random()),
            Err(CredentialMismatch::Node { .. })
        ));
    }
}
