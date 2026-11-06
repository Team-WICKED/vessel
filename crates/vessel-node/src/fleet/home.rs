//! The home role: publishing the bundle made from `fleet.toml`'s sources
//! ([`Home::publish`]) and serving `fleet.manifest`, `fleet.blob`,
//! `fleet.watch`, `fleet.report` and `fleet.members`.
//!
//! [Decision Log]
//! - Noticing edits: the home looks at its sources every few seconds
//!   (`FleetConfig::poll`), but bundles only when a cheap stamp changed
//!   (every entry's path, kind, size and modification time, and
//!   `fleet.toml`'s text), and at least every 10 minutes regardless.
//!   Bundling copies every source file into the blob store, so doing it
//!   on every poll would rewrite the store several times a minute.
//!   Alternatives: a filesystem watcher (`notify`: a new dependency, and
//!   events through the operator's symlinked layout are unreliable);
//!   bundling only on a command (`vessel fleet push` is not in protocol
//!   5, and an edit would wait for someone to remember it).
//! - The stamp follows a symlink only where the bundle would (inside the
//!   skills directory), so a link to `/` is never walked.
//! - `fleet.watch` is a long poll on a `watch` channel of the generation:
//!   it answers at once if the member's generation differs from the
//!   home's (newer, or older after the home's data was reset), else when
//!   one is published, or with the current one when `watch_limit` (50 s)
//!   passes or the node stops. A secure connection is pinged every 10 s,
//!   so a waiting call never looks idle.
//! - `fleet.blob` serves only blobs of the current manifest: a member
//!   cannot read anything else from the store by guessing hashes, and a
//!   member that raced a new publication gets `not_found` and fetches the
//!   manifest again.
//! - Reports are kept per member device in `members.json`; `fleet.members`
//!   lists the devices holding the `sync` scope, revoked ones left out,
//!   with their last report.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use vessel_fleet::{BlobStore, PastEnd, Sources};
use vessel_proto::{
    BlobChunk, BlobHash, BundlePath, DeviceId, DeviceInfo, ErrorCode, FleetMember, Generation,
    Manifest, ReportApplied, Scope, WireError,
};

use super::files::{self, FleetFiles};

/// A published bundle.
#[derive(Debug)]
pub(crate) struct Published {
    pub(crate) manifest: Manifest,
    hashes: BTreeSet<BlobHash>,
    pub(crate) blobs: BlobStore,
}

impl Published {
    pub(crate) fn new(manifest: Manifest, blobs: BlobStore) -> Self {
        let hashes = manifest.files.values().map(|file| file.hash).collect();
        Self {
            manifest,
            hashes,
            blobs,
        }
    }
}

/// What a member last reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Report {
    applied: Generation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<BundlePath>,
    reported_at: DateTime<Utc>,
}

/// The home's side of the fleet.
#[derive(Debug)]
pub(crate) struct Home {
    files: FleetFiles,
    published: watch::Sender<Option<Arc<Published>>>,
    reports: Mutex<BTreeMap<DeviceId, Report>>,
    watch_limit: Duration,
    stop: CancellationToken,
}

fn unavailable(message: &str) -> WireError {
    WireError::new(ErrorCode::Unavailable, message.to_owned())
}

fn internal(error: &impl std::fmt::Display) -> WireError {
    WireError::new(ErrorCode::Internal, error.to_string())
}

impl Home {
    /// The home of the node keeping its state in `files`, with nothing
    /// published yet. Reads `members.json`; blocks.
    pub(crate) fn new(files: FleetFiles, watch_limit: Duration, stop: CancellationToken) -> Self {
        let reports = files::read_json(&files.members())
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "member reports unreadable; starting without them");
                None
            })
            .unwrap_or_default();
        Self {
            files,
            published: watch::Sender::new(None),
            reports: Mutex::new(reports),
            watch_limit,
            stop,
        }
    }

    /// Serve `published` from now on.
    pub(crate) fn publish(&self, published: Option<Arc<Published>>) {
        let _previous = self.published.send_replace(published);
    }

    /// What is published now.
    pub(crate) fn current(&self) -> Option<Arc<Published>> {
        self.published.borrow().clone()
    }

    fn require(&self) -> Result<Arc<Published>, WireError> {
        self.current().ok_or_else(|| {
            unavailable(
                "this node is not a fleet home, or has no bundle yet (`vessel fleet status`)",
            )
        })
    }

    /// `fleet.manifest`.
    pub(crate) fn manifest(&self) -> Result<Manifest, WireError> {
        Ok(self.require()?.manifest.clone())
    }

    /// `fleet.blob`.
    pub(crate) async fn blob(&self, hash: BlobHash, offset: u64) -> Result<BlobChunk, WireError> {
        let published = self.require()?;
        if !published.hashes.contains(&hash) {
            return Err(WireError::new(
                ErrorCode::NotFound,
                format!("blob {hash} is not in the current bundle"),
            ));
        }
        let blobs = published.blobs.clone();
        let read = tokio::task::spawn_blocking(move || {
            blobs.read_chunk(&hash, offset, BlobChunk::MAX_LEN)
        })
        .await
        .map_err(|error| internal(&error))?;
        match read {
            Ok((data, total)) => Ok(BlobChunk {
                data: data.into(),
                total,
            }),
            Err(error)
                if error
                    .get_ref()
                    .is_some_and(<dyn std::error::Error + Send + Sync>::is::<PastEnd>) =>
            {
                Err(WireError::new(ErrorCode::InvalidRequest, error.to_string()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(WireError::new(
                ErrorCode::NotFound,
                format!("blob {hash} is not in the store"),
            )),
            Err(error) => Err(internal(&error)),
        }
    }

    /// `fleet.watch`.
    pub(crate) async fn watch(&self, after: Generation) -> Result<Generation, WireError> {
        let mut receiver = self.published.subscribe();
        let generation = |published: &Option<Arc<Published>>| {
            published
                .as_ref()
                .map(|published| published.manifest.generation)
        };
        if generation(&receiver.borrow_and_update()).is_none() {
            return Err(unavailable("this node is not a fleet home"));
        }
        let changed = receiver.wait_for(|published| {
            generation(published).is_some_and(|generation| generation != after)
        });
        tokio::select! {
            _waited = tokio::time::timeout(self.watch_limit, changed) => {}
            () = self.stop.cancelled() => {}
        }
        generation(&self.published.borrow())
            .ok_or_else(|| unavailable("this node stopped being a fleet home"))
    }

    /// `fleet.report` from `device`.
    pub(crate) async fn report(
        &self,
        device: DeviceId,
        request: ReportApplied,
    ) -> Result<(), WireError> {
        let mut reports = self.reports.lock().await;
        let _previous = reports.insert(
            device,
            Report {
                applied: request.generation,
                conflicts: request.conflicts,
                reported_at: Utc::now(),
            },
        );
        let snapshot = reports.clone();
        let path = self.files.members();
        tokio::task::spawn_blocking(move || files::write_json(&path, &snapshot))
            .await
            .map_err(|error| internal(&error))?
            .map_err(|error| internal(&error))?;
        drop(reports);
        Ok(())
    }

    /// `fleet.members`, given the node's devices.
    pub(crate) async fn members(&self, devices: Vec<DeviceInfo>) -> Vec<FleetMember> {
        let reports = self.reports.lock().await;
        devices
            .into_iter()
            .filter(|device| device.scopes.contains(&Scope::Sync) && device.revoked_at.is_none())
            .map(|device| {
                let report = reports.get(&device.id);
                FleetMember {
                    device: device.id,
                    label: device.label,
                    applied: report.map(|report| report.applied),
                    conflicts: report
                        .map(|report| report.conflicts.clone())
                        .unwrap_or_default(),
                    reported_at: report.map(|report| report.reported_at),
                }
            })
            .collect()
    }
}

/// When the sources were last looked at, in a form cheap to compare: every
/// entry's path, kind, size and modification time, and `fleet.toml`'s
/// text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Stamp {
    toml: String,
    entries: Vec<(PathBuf, bool, u64, Option<SystemTime>)>,
}

/// Entries a stamp looks at, at most: a skills directory larger than this
/// is still bundled, on the 10-minute timer.
const STAMP_LIMIT: usize = 50_000;

impl Stamp {
    /// Look at `sources` (blocking).
    pub(crate) fn of(toml: &str, sources: &Sources) -> Self {
        let mut stamp = Self {
            toml: toml.to_owned(),
            entries: Vec::new(),
        };
        if let Some(instructions) = &sources.instructions {
            stamp.entry(instructions, std::fs::metadata(instructions).ok());
        }
        if let Some(skills) = &sources.skills
            && let Ok(root) = std::fs::canonicalize(skills)
        {
            let mut visited = BTreeSet::new();
            stamp.walk(&root, &root, &mut visited);
        }
        stamp
    }

    fn entry(&mut self, path: &Path, metadata: Option<std::fs::Metadata>) {
        let (dir, size, modified) = metadata.map_or((false, 0, None), |metadata| {
            (metadata.is_dir(), metadata.len(), metadata.modified().ok())
        });
        self.entries.push((path.to_path_buf(), dir, size, modified));
    }

    fn walk(&mut self, root: &Path, dir: &Path, visited: &mut BTreeSet<PathBuf>) {
        if !visited.insert(dir.to_path_buf()) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        // Sorted, so the same tree always stamps the same.
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if self.entries.len() >= STAMP_LIMIT {
                return;
            }
            // Followed only where the bundle follows: inside the skills
            // directory.
            let resolved = match std::fs::canonicalize(&path) {
                Ok(resolved) if resolved.starts_with(root) => resolved,
                Ok(_) | Err(_) => {
                    self.entry(&path, std::fs::symlink_metadata(&path).ok());
                    continue;
                }
            };
            let metadata = std::fs::metadata(&resolved).ok();
            let is_dir = metadata.as_ref().is_some_and(std::fs::Metadata::is_dir);
            self.entry(&path, metadata);
            if is_dir {
                self.walk(root, &resolved, visited);
            }
        }
    }
}
