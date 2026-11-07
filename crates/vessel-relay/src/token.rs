//! Reading a relay registration token from a file: `vesseld relay
//! --token-file`, and a node's `--relay-token-file`.
//!
//! The token is a secret shared by the relay and the nodes that use it
//! (plan, decision 4). A file, not a command-line argument, so it never
//! shows in process listings or shell history; surrounding whitespace is
//! ignored, so `echo token > file` works. A file others can read is
//! reported (on Unix), not refused: where it lives is the operator's call.

use std::io;
use std::path::{Path, PathBuf};

use vessel_proto::RelayToken;

/// Why a token file could not be used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TokenFileError {
    /// The file could not be read.
    #[error("cannot read the relay token file {path}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The file holds no token.
    #[error("the relay token file {path} is empty")]
    Empty {
        /// The file.
        path: PathBuf,
    },
}

/// The token in `path`.
///
/// # Errors
/// The file cannot be read or holds only whitespace.
pub fn read_token_file(path: &Path) -> Result<RelayToken, TokenFileError> {
    let text = std::fs::read_to_string(path).map_err(|source| TokenFileError::Read {
        path: path.to_owned(),
        source,
    })?;
    warn_if_shared(path);
    RelayToken::new(text.trim()).map_err(|_empty| TokenFileError::Empty {
        path: path.to_owned(),
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
            "the relay token file is readable by other users; consider chmod 600"
        );
    }
}

#[cfg(not(unix))]
fn warn_if_shared(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_read_trimmed_and_an_empty_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("vessel-relay-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, "  secret-token \n").unwrap();
        assert_eq!(read_token_file(&file).unwrap().expose(), "secret-token");
        std::fs::write(&file, " \n").unwrap();
        assert!(matches!(
            read_token_file(&file),
            Err(TokenFileError::Empty { .. })
        ));
        assert!(matches!(
            read_token_file(&dir.join("missing")),
            Err(TokenFileError::Read { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
