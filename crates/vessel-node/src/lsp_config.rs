//! Explicit operator configuration, never discovered in a queried repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::io::AsyncReadExt as _;
use vessel_lsp::registry::Override;
use vessel_proto::lsp::Language;

use crate::{Config, LspRoot};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default)]
    roots: Vec<LspRoot>,
    #[serde(default)]
    servers: BTreeMap<Language, Override>,
    typescript_server: Option<PathBuf>,
    rust_toolchain: Option<PathBuf>,
    cargo_home: Option<PathBuf>,
    python_interpreter: Option<PathBuf>,
}

/// Why an explicitly selected LSP configuration could not be used.
#[derive(Debug, thiserror::Error)]
pub enum LspConfigError {
    /// File could not be read.
    #[error("cannot read LSP configuration")]
    Io(#[from] std::io::Error),
    /// JSON did not match the operator schema.
    #[error("invalid LSP configuration")]
    Json(#[from] serde_json::Error),
    /// Oversized configuration.
    #[error("LSP configuration exceeds 64 KiB")]
    TooLarge,
}

/// Load a bounded, explicit operator file into node configuration.
///
/// No file is discovered through the workspace or received over RPC.
///
/// # Errors
/// Fails on unreadable, oversized or invalid operator configuration.
pub async fn load_lsp_config(config: &mut Config, path: &Path) -> Result<(), LspConfigError> {
    let mut bytes = Vec::new();
    let _read = tokio::fs::File::open(path)
        .await?
        .take(65_537)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > 65_536 {
        return Err(LspConfigError::TooLarge);
    }
    let settings: Settings = serde_json::from_slice(&bytes)?;
    config.lsp_roots = settings.roots;
    config.lsp.registry.overrides = settings.servers;
    config.lsp.registry.typescript_server = settings.typescript_server;
    config.lsp.registry.rust_toolchain = settings.rust_toolchain;
    config.lsp.registry.cargo_home = settings.cargo_home;
    config.lsp.registry.python_interpreter = settings.python_interpreter;
    Ok(())
}
