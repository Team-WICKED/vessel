//! This node's secrets: `<data_dir>/secrets.toml` (ADR 0012, "Secrets stay
//! on each node"), what fleet MCP server definitions refer to by name.
//!
//! ```toml
//! # one secret per line: NAME = "value"
//! DOCS_TOKEN = "..."
//! ```
//!
//! [Decision Log]
//! - Purpose: a definition travels between nodes with `{ secret = "NAME" }`
//!   in place of the value; each node fills it from this file when it
//!   launches a harness, so a value never leaves the node it was set on.
//! - Format: a flat table of names (the environment variable rule,
//!   `SecretName`) to strings. Nothing else is accepted, so a typo cannot
//!   turn into a literal.
//! - Errors never show a value: a file that does not parse is reported by
//!   line only (the TOML parser's own message quotes the line, which may
//!   hold a secret), and a wrong entry by its name only.
//! - Owner-only. `set` and `delete` write it 0600 and atomically, through
//!   `toml_edit` so comments and the other entries stay as they were, under
//!   the fleet lock so two of them never lose each other's change. A file
//!   others can read is still used, with a warning, as for `token` and
//!   `profiles.toml`: the owner wrote it and decides.
//! - Values enter only through `vessel fleet secret set` (a prompt without
//!   echo, or standard input), never a command line. `set_secret`,
//!   `secret_names` and `delete_secret` are what protocol 6's `secret.set`,
//!   `secret.list` and `secret.delete` (admin) will call; there is no way
//!   to read a value back out, by design.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr as _;

use vessel_proto::{ResolvedValue, SecretName};

use super::files::{FileError, FleetFiles, FleetLock};

/// The secrets of this node, by name. `Debug` shows names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Secrets(BTreeMap<SecretName, ResolvedValue>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_set().entries(self.0.keys()).finish()
    }
}

impl Secrets {
    /// The value of `name`, if set.
    pub(crate) fn get(&self, name: &SecretName) -> Option<ResolvedValue> {
        self.0.get(name).cloned()
    }

    /// The names set.
    pub(crate) fn names(&self) -> impl Iterator<Item = &SecretName> {
        self.0.keys()
    }
}

/// Why `secrets.toml` cannot be used. Never holds a value.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    /// It could not be read or written.
    #[error(transparent)]
    File(#[from] FileError),
    /// It is not TOML; where.
    #[error("{}: not TOML near line {line} (the line is not shown: it may hold a secret)", path.display())]
    Parse {
        /// The file.
        path: std::path::PathBuf,
        /// The line (from 1).
        line: usize,
    },
    /// An entry that is not `NAME = "text"`.
    #[error("{}: `{key}` must be a secret name (letters, digits, `_`) set to a string", path.display())]
    Entry {
        /// The file.
        path: std::path::PathBuf,
        /// The key as written.
        key: String,
    },
}

/// The secrets in `files`' `secrets.toml` (none if there is no file),
/// and the SHA-256 of its text, to tell a change. Blocks.
pub(crate) fn read(files: &FleetFiles) -> Result<(Secrets, [u8; 32]), SecretsError> {
    let path = files.secrets();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Secrets::default(), [0; 32]));
        }
        Err(error) => return Err(FileError::io(path, error).into()),
    };
    let digest = <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes()).into();
    let document = parse(path, &text)?;
    let mut secrets = BTreeMap::new();
    for (key, item) in document.iter() {
        let entry = || SecretsError::Entry {
            path: path.to_owned(),
            key: key.to_owned(),
        };
        let name = SecretName::from_str(key).map_err(|_invalid| entry())?;
        let value = item.as_str().ok_or_else(entry)?;
        let _previous = secrets.insert(name, ResolvedValue::new(value.to_owned()));
    }
    warn_if_shared(path);
    Ok((Secrets(secrets), digest))
}

fn parse(path: &Path, text: &str) -> Result<toml_edit::DocumentMut, SecretsError> {
    text.parse::<toml_edit::DocumentMut>().map_err(|error| {
        let at = error.span().map_or(0, |span| span.start);
        let line = text
            .get(..at)
            .map_or(1, |before| before.matches('\n').count().saturating_add(1));
        SecretsError::Parse {
            path: path.to_owned(),
            line,
        }
    })
}

#[cfg(unix)]
fn warn_if_shared(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(
            path = %path.display(),
            "secrets.toml can be read by other users; `chmod 600` it"
        );
    }
}

#[cfg(not(unix))]
fn warn_if_shared(_path: &Path) {}

/// The names set in `secrets.toml`. Blocks.
///
/// # Errors
/// See [`SecretsError`].
pub fn secret_names(files: &FleetFiles) -> Result<Vec<SecretName>, SecretsError> {
    let (secrets, _digest) = read(files)?;
    Ok(secrets.names().cloned().collect())
}

/// Set secret `name` to `value` in `secrets.toml` (created owner-only),
/// keeping everything else in it. A running node uses it within seconds,
/// for sessions it starts from then on. Blocks.
///
/// # Errors
/// See [`SecretsError`].
pub fn set_secret(files: &FleetFiles, name: &SecretName, value: &str) -> Result<(), SecretsError> {
    edit(files, |document| {
        let _previous = document.insert(name.as_str(), toml_edit::value(value));
        true
    })
    .map(drop)
}

/// Delete secret `name` from `secrets.toml`; whether it was set. Blocks.
///
/// # Errors
/// See [`SecretsError`].
pub fn delete_secret(files: &FleetFiles, name: &SecretName) -> Result<bool, SecretsError> {
    edit(files, |document| document.remove(name.as_str()).is_some())
}

/// Change `secrets.toml` under the fleet lock with `change`, which says
/// whether it changed anything; written only then.
fn edit(
    files: &FleetFiles,
    change: impl FnOnce(&mut toml_edit::DocumentMut) -> bool,
) -> Result<bool, SecretsError> {
    let _lock = FleetLock::acquire(files)?;
    let path = files.secrets();
    let mut document = match std::fs::read_to_string(path) {
        Ok(text) => parse(path, &text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut document = toml_edit::DocumentMut::new();
            document.decor_mut().set_prefix(
                "# This node's secrets for the fleet's MCP servers (ADR 0012): NAME = \"value\".\n\
                 # Owner-only. Vessel never copies them anywhere; set them with\n\
                 # `vessel fleet secret set NAME`.\n",
            );
            document
        }
        Err(error) => return Err(FileError::io(path, error).into()),
    };
    if !change(&mut document) {
        return Ok(false);
    }
    vessel_fleet::write_private(path, document.to_string().as_bytes())
        .map_err(|error| FileError::io(path, error))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use vessel_store::DataDir;

    use super::*;

    fn files() -> (tempfile::TempDir, FleetFiles) {
        let temp = tempfile::tempdir().unwrap();
        let files = FleetFiles::new(&DataDir::new(temp.path()));
        (temp, files)
    }

    fn name(text: &str) -> SecretName {
        text.parse().unwrap()
    }

    #[test]
    fn set_and_remove_keep_comments_and_other_entries() {
        let (_temp, files) = files();
        assert!(read(&files).unwrap().0.names().next().is_none());
        set_secret(&files, &name("DOCS_TOKEN"), "first \"quoted\" value").unwrap();
        let text = std::fs::read_to_string(files.secrets()).unwrap();
        std::fs::write(files.secrets(), format!("{text}# mine\nOTHER = \"kept\"\n")).unwrap();
        set_secret(&files, &name("DOCS_TOKEN"), "second").unwrap();
        let (secrets, _digest) = read(&files).unwrap();
        assert_eq!(secrets.get(&name("DOCS_TOKEN")).unwrap().expose(), "second");
        assert_eq!(secrets.get(&name("OTHER")).unwrap().expose(), "kept");
        assert!(
            std::fs::read_to_string(files.secrets())
                .unwrap()
                .contains("# mine")
        );
        assert!(delete_secret(&files, &name("DOCS_TOKEN")).unwrap());
        assert!(!delete_secret(&files, &name("DOCS_TOKEN")).unwrap());
        assert_eq!(secret_names(&files).unwrap(), [name("OTHER")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(files.secrets())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(
            !format!("{secrets:?}").contains("kept"),
            "Debug shows names only"
        );
    }

    #[test]
    fn errors_never_show_a_value() {
        let (_temp, files) = files();
        for (text, shown) in [
            ("A = \"hunter2\nB = 1\n", "hunter2"),
            ("A = 'hunter2'\nB = 7357\n", "7357"),
            ("bad-name = \"hunter2\"\n", "hunter2"),
            ("[table]\nA = \"hunter2\"\n", "hunter2"),
        ] {
            std::fs::write(files.secrets(), text).unwrap();
            let error = read(&files).unwrap_err().to_string();
            assert!(!error.contains(shown), "{error}");
        }
        std::fs::write(files.secrets(), "A = \"ok\"\nB = \"broken\n").unwrap();
        assert!(matches!(
            read(&files),
            Err(SecretsError::Parse { line: 2, .. })
        ));
    }
}
