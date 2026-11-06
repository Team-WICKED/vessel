//! Device credentials on the client (ADR 0010): what a paired device
//! presents to its node, one file per node.
//!
//! [Decision Log]
//! - Purpose: a paired device authenticates with its device id and a
//!   256-bit secret issued at pairing. It must survive restarts, and must
//!   not end up where the node book does (the book is edited by hand and
//!   may be shared between machines).
//! - Choice: `<config_dir>/vessel/credentials/<node id>.secret`, a small
//!   TOML file (`device`, `secret`), owner-only on Unix (directory 0700,
//!   file 0600). The book points to it (`credential = { file = ... }`).
//!   Named by node id, so pairing the same node again replaces it.
//! - Written under a temporary name and renamed into place, so a crash
//!   leaves the old credential or the new one, never half of one.
//! - Windows: the files inherit the user profile's access control, which
//!   keeps other users out; no extra ACL code.
//! - Moving a device means pairing it again, not copying this file (ADR
//!   0010); it is bearer material, like the node's own key.

use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use vessel_proto::{DeviceId, DeviceSecret, NodeId};

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
    /// The device, as the node knows it.
    pub device: DeviceId,
    /// Its secret.
    pub secret: DeviceSecret,
}

impl fmt::Debug for DeviceCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCredential")
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

impl DeviceCredential {
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
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        let text = format!(
            "# A Vessel device credential. Keep it secret; to move this device,\n\
             # pair it again, and revoke it on the node if it is lost.\n\
             device = \"{}\"\nsecret = \"{}\"\n",
            self.device, self.secret
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
        let credential = DeviceCredential {
            device: DeviceId::random(),
            secret: DeviceSecret::from_bytes([8; 32]),
        };
        credential.write(&path).unwrap();
        assert_eq!(DeviceCredential::read(&path).unwrap(), credential);
        assert!(!format!("{credential:?}").contains(&credential.secret.to_string()));

        let replaced = DeviceCredential {
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
    }
}
