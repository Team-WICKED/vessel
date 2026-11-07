//! Saved snapshots and file URI confinement, before any source file is read.
//!
//! Canonical paths stop ordinary traversal and symlink escape. The trusted
//! workspace can still change between canonicalization and open: this is an
//! API policy boundary, not a filesystem sandbox against a hostile OS user.

use std::path::{Path, PathBuf};

use tokio::io::AsyncReadExt;
use url::Url;
use vessel_proto::lsp::FilePath;

use crate::error::LspError as Error;

/// The largest saved document synchronized to a server (2 MiB).
pub(crate) const MAX_FILE: u64 = 2 * 1024 * 1024;

/// Canonicalize an authorized root without permitting network paths.
///
/// # Errors
/// Rejects relative/network paths, missing roots and non-directories.
pub async fn canonical_root(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() || network_path(path) {
        return Err(Error::Path);
    }
    let path = tokio::fs::canonicalize(path).await?;
    if !tokio::fs::metadata(&path).await?.is_dir() {
        return Err(Error::Path);
    }
    Ok(path)
}

fn network_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.starts_with("//")
        || text.starts_with("\\\\") && !text.starts_with("\\\\?\\")
        || text.starts_with("\\\\?\\UNC\\")
}

/// Resolve a relative file below the root, verifying symlinks before reading.
pub(crate) async fn resolve(root: &Path, file: &FilePath) -> Result<PathBuf, Error> {
    let candidate = root.join(file.as_path());
    let canonical = tokio::fs::canonicalize(candidate).await?;
    if !canonical.starts_with(root) || !tokio::fs::metadata(&canonical).await?.is_file() {
        return Err(Error::Path);
    }
    Ok(canonical)
}

pub(crate) fn uri(path: &Path) -> Result<String, Error> {
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|()| Error::Path)
}

pub(crate) async fn from_uri(root: &Path, given: &str) -> Result<(FilePath, PathBuf), Error> {
    let url = Url::parse(given)?;
    if url.scheme() != "file"
        || url
            .host_str()
            .is_some_and(|host| !host.is_empty() && host != "localhost")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Path);
    }
    let path = url.to_file_path().map_err(|()| Error::Path)?;
    if network_path(&path) || !lexically_inside(&path, root) {
        return Err(Error::Path);
    }
    let path = tokio::fs::canonicalize(path).await?;
    let relative = relative(root, &path)?;
    if !tokio::fs::metadata(&path).await?.is_file() {
        return Err(Error::Path);
    }
    Ok((relative, path))
}

pub(crate) fn relative(root: &Path, path: &Path) -> Result<FilePath, Error> {
    let relative = path.strip_prefix(root).map_err(|_outside| Error::Path)?;
    // Wire paths use '/' on every platform, even when canonical Windows
    // paths use a verbatim drive prefix and backslash separators.
    let names = relative
        .components()
        .map(|part| match part {
            std::path::Component::Normal(name) => name.to_str().ok_or(Error::Path),
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::CurDir
            | std::path::Component::ParentDir => Err(Error::Path),
        })
        .collect::<Result<Vec<_>, _>>()?;
    FilePath::try_from(PathBuf::from(names.join("/"))).map_err(|_invalid| Error::Path)
}

pub(crate) async fn read(path: &Path) -> Result<String, Error> {
    let file = tokio::fs::File::open(path).await?;
    if !file.metadata().await?.is_file() {
        return Err(Error::Path);
    }
    let mut bytes = Vec::new();
    let _read = file
        .take(MAX_FILE.saturating_add(1))
        .read_to_end(&mut bytes)
        .await?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE {
        return Err(Error::Limit);
    }
    Ok(String::from_utf8(bytes)?)
}

#[cfg(not(windows))]
fn lexically_inside(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(windows)]
fn lexically_inside(path: &Path, root: &Path) -> bool {
    // canonicalize returns a verbatim drive prefix, whereas a file URI
    // decodes to an ordinary drive path. The later canonical comparison is
    // still authoritative; this lexical gate only avoids unrelated lookups.
    let root = root
        .to_str()
        .and_then(|text| text.strip_prefix("\\\\?\\"))
        .map_or(root, Path::new);
    let mut candidate = path.components();
    root.components().all(|expected| {
        candidate.next().is_some_and(|part| {
            part.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_uris_roundtrip_spaces_unicode_and_windows_verbatim_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(dir.path()).await.unwrap();
        let path = root.join("space #😀%.rs");
        tokio::fs::write(&path, "source").await.unwrap();
        let (file, resolved) = from_uri(&root, &uri(&path).unwrap()).await.unwrap();
        assert_eq!(resolved, tokio::fs::canonicalize(&path).await.unwrap());
        assert_eq!(file.as_path(), Path::new("space #😀%.rs"));
        let outside = tempfile::tempdir().unwrap();
        assert!(
            from_uri(&root, &uri(outside.path()).unwrap())
                .await
                .is_err()
        );
    }
}
