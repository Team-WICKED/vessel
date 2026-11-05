//! [`discover`]: which harnesses are installed, and their versions.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _};
use vessel_proc::{CommandSpec, Spawned, spawn};
use vessel_proto::{Harness, HarnessKind, StructuredSupport};

use crate::harnesses::{Structured, descriptor};
use crate::launch::adapter_missing;
use crate::lookup::Lookup;

/// How long a harness gets to answer `--version`.
///
/// Some harnesses check for updates or start a runtime before printing;
/// five seconds covers a cold start without stalling `harness.list` when one
/// hangs.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Most output read from a version probe. A version is one short line; the
/// cap keeps a misbehaving binary from filling memory.
const PROBE_OUTPUT_LIMIT: u64 = 4096;

/// Every known harness installed on the search path, in
/// [`HarnessKind::ALL`] order.
///
/// Each one is asked for its version concurrently with a timeout; a harness
/// that does not answer in time (or fails) is still reported, with
/// `version: None`. Never fails: a broken harness is simply reported with
/// what is known about it.
pub async fn discover(lookup: &Lookup) -> Vec<Harness> {
    let lookup = lookup.clone();
    let found = match tokio::task::spawn_blocking(move || locate_all(&lookup)).await {
        Ok(found) => found,
        Err(error) => {
            tracing::error!(%error, "harness lookup task failed");
            return Vec::new();
        }
    };
    futures_util::future::join_all(found.into_iter().map(|found| async move {
        let version = probe_version(&found.path).await;
        tracing::debug!(
            harness = %found.kind,
            path = %found.path.display(),
            ?version,
            "harness found"
        );
        Harness {
            kind: found.kind,
            path: found.path,
            version,
            structured: found.structured,
        }
    }))
    .await
}

/// An installed harness before its version is known.
struct Found {
    kind: HarnessKind,
    path: PathBuf,
    structured: StructuredSupport,
}

/// Filesystem part of discovery; blocking.
fn locate_all(lookup: &Lookup) -> Vec<Found> {
    HarnessKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let descriptor = descriptor(kind);
            let path = lookup.find(descriptor.binary)?;
            let structured = match &descriptor.structured {
                Structured::Native { .. } => StructuredSupport::Available,
                Structured::Adapter { binary, install } => {
                    if lookup.find(binary).is_some() {
                        StructuredSupport::Available
                    } else {
                        StructuredSupport::Unavailable {
                            reason: adapter_missing(binary, install),
                        }
                    }
                }
            };
            Some(Found {
                kind,
                path,
                structured,
            })
        })
        .collect()
}

/// First non-empty line of `<path> --version`, if it exits 0 in time.
///
/// On timeout the probe's `Child` is dropped, which kills its whole process
/// group.
async fn probe_version(path: &Path) -> Option<String> {
    let Spawned {
        mut child,
        stdin,
        stdout,
        stderr,
    } = spawn(&CommandSpec::new(path).arg("--version"))
        .inspect_err(|error| tracing::debug!(%error, "version probe did not start"))
        .ok()?;
    drop(stdin);
    let probe = async {
        // stderr is drained so a chatty probe cannot block on a full pipe.
        let (stdout, _stderr, code) =
            tokio::join!(read_limited(stdout), read_limited(stderr), child.wait());
        (stdout, code)
    };
    let Ok((stdout, code)) = tokio::time::timeout(PROBE_TIMEOUT, probe).await else {
        tracing::debug!(path = %path.display(), "version probe timed out");
        return None;
    };
    match code {
        Ok(Some(0)) => first_line(&stdout),
        Ok(code) => {
            tracing::debug!(path = %path.display(), ?code, "version probe failed");
            None
        }
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "version probe failed");
            None
        }
    }
}

async fn read_limited(reader: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Err(error) = reader
        .take(PROBE_OUTPUT_LIMIT)
        .read_to_end(&mut buffer)
        .await
    {
        tracing::debug!(%error, "version probe output unreadable");
    }
    buffer
}

fn first_line(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_the_first_non_empty_line() {
        assert_eq!(
            first_line(b"\n  \n 2.1.0 (Claude Code)\nextra\n").as_deref(),
            Some("2.1.0 (Claude Code)")
        );
        assert_eq!(first_line(b"\n\n"), None);
    }
}
