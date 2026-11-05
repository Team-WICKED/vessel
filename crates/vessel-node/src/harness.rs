//! Discovery of installed agent harnesses.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use vessel_core::{Harness, HarnessKind};

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve a harness binary on `PATH`.
pub(crate) fn locate(kind: HarnessKind) -> Option<PathBuf> {
    which::which(kind.binary()).ok()
}

/// Find every known harness installed on this machine.
///
/// Each one found is asked for its version with a short timeout; a harness
/// that does not answer is still reported, with `version: None`.
pub async fn discover() -> Vec<Harness> {
    let mut found = Vec::new();
    for kind in HarnessKind::ALL {
        let Some(path) = locate(kind) else {
            continue;
        };
        let version = probe_version(&path).await;
        tracing::debug!(%kind, path = %path.display(), ?version, "harness found");
        found.push(Harness {
            kind,
            path,
            version,
        });
    }
    found
}

async fn probe_version(path: &Path) -> Option<String> {
    let run = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(VERSION_PROBE_TIMEOUT, run)
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}
