//! `session.import` and `memory.import`: the target pulls a transfer from
//! its [`Source`], renames it for its own working directory, and places it
//! into one of its profiles' homes.
//!
//! [Decision Log]
//! - Purpose: the target decides where things land (its profile, its
//!   directory), so the target pulls (protocol 6, "target pull"); the
//!   source only snapshots and serves.
//! - [`Source`] is the one seam between "this node" and "another node":
//!   [`Local`] calls this node's own export, read and close for the
//!   caller (ownership checked as `session.export` would be); Track X's
//!   envelope implements the same four calls with `fleet.forward`. The
//!   import does not know which it has.
//! - Order: export there; check the manifest against the limits before
//!   fetching anything; fetch every file into an incoming staging store,
//!   each checked against its hash; rename each path for the target's
//!   directory (`vessel_harness::rebase`); place; close the source's
//!   transfer whatever happened, and remove the incoming store.
//! - A session is placed whole or not at all (`OnConflict::Refuse`): a
//!   conversation half replaced would be neither. Memory goes beside
//!   what differs (`OnConflict::Beside`), as the fleet's materialiser does.
//! - A session imported into the very profile and directory it is in on
//!   this node is refused: every file is already there, and a second
//!   session resuming it would append to the same transcript as the first.
//! - Resuming (`start`) is the service's: it creates the session through
//!   `session.create`'s own path (the agent limits apply), recording
//!   `origin: imported { node, session }`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vessel_fleet::{Incoming, OnConflict, Placed};
use vessel_harness::rebase;
use vessel_proto::{
    BlobChunk, BundleFile, BundlePath, CloseTransfer, ExportMemory, ExportSession, HarnessKind,
    ImportMemory, ImportSession, MemoryImported, NodeId, ReadTransfer, SessionImported,
    SessionMode, SessionOrigin, SessionSpec, TransferDetail, TransferId, TransferManifest,
    WireError,
};

use super::export::{Exporter, home, named};
use super::{Holder, TransferError, Transfers, check_limits, discard, wire};
use crate::error::ServiceError;
use crate::fleet::FetchError;
use crate::profiles::NodeProfiles;

/// Where an import pulls from: the four calls of protocol 6 a source
/// answers. See the module's Decision Log.
pub(crate) trait Source {
    /// `session.export` there.
    async fn export_session(&self, request: ExportSession) -> Result<TransferManifest, WireError>;
    /// `memory.export` there.
    async fn export_memory(&self, request: ExportMemory) -> Result<TransferManifest, WireError>;
    /// `transfer.read` there.
    async fn read(&self, request: ReadTransfer) -> Result<BlobChunk, WireError>;
    /// `transfer.close` there.
    async fn close(&self, request: CloseTransfer) -> Result<(), WireError>;
}

/// This node as the source, for `holder`.
#[derive(Debug)]
pub(crate) struct Local<'a> {
    pub(crate) exporter: Exporter<'a>,
    pub(crate) holder: Holder,
}

impl Source for Local<'_> {
    async fn export_session(&self, request: ExportSession) -> Result<TransferManifest, WireError> {
        self.exporter
            .session(self.holder, request)
            .await
            .map_err(|error| error.to_wire())
    }

    async fn export_memory(&self, request: ExportMemory) -> Result<TransferManifest, WireError> {
        self.exporter
            .memory(self.holder, request)
            .await
            .map_err(|error| error.to_wire())
    }

    async fn read(&self, request: ReadTransfer) -> Result<BlobChunk, WireError> {
        self.exporter.transfers.read(self.holder, request).await
    }

    async fn close(&self, request: CloseTransfer) -> Result<(), WireError> {
        self.exporter
            .transfers
            .close(self.holder, request.transfer)
            .await
            .map_err(|error| wire(&error))
    }
}

/// What imports need of the node.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Importer<'a> {
    pub(crate) profiles: &'a NodeProfiles,
    pub(crate) transfers: &'a Arc<Transfers>,
    /// This node.
    pub(crate) node: NodeId,
}

/// A session placed in a profile's home: the answer without a session, and
/// what would resume it.
#[derive(Debug)]
pub(crate) struct SessionPlaced {
    pub(crate) imported: SessionImported,
    /// A structured session resuming the conversation where it now is.
    pub(crate) spec: SessionSpec,
    /// Where it came from.
    pub(crate) origin: SessionOrigin,
}

fn to_wire(error: impl Into<ServiceError>) -> WireError {
    error.into().to_wire()
}

impl Importer<'_> {
    /// `session.import` from `source`, without starting it.
    pub(crate) async fn session<S: Source>(
        &self,
        source: &S,
        request: &ImportSession,
    ) -> Result<SessionPlaced, WireError> {
        if let Some(cwd) = &request.cwd {
            directory(cwd, request.start).map_err(to_wire)?;
        }
        // An unknown profile is refused before the source is asked.
        let _known = named(self.profiles, None, request.profile.as_ref()).map_err(to_wire)?;
        let manifest = source
            .export_session(ExportSession {
                session: request.from.session,
                force: false,
            })
            .await?;
        let transfer = manifest.transfer;
        let placed = self.place_session(source, manifest, request).await;
        closed(source, transfer).await;
        placed
    }

    async fn place_session<S: Source>(
        &self,
        source: &S,
        manifest: TransferManifest,
        request: &ImportSession,
    ) -> Result<SessionPlaced, WireError> {
        let TransferManifest {
            transfer,
            files,
            detail,
        } = manifest;
        let TransferDetail::Session {
            harness_session,
            cwd: from_cwd,
            info,
        } = detail
        else {
            return Err(to_wire(TransferError::WrongKind("memory")));
        };
        let harness = info.spec.harness;
        let profile =
            named(self.profiles, Some(harness), request.profile.as_ref()).map_err(to_wire)?;
        let to_cwd = request.cwd.clone().unwrap_or_else(|| from_cwd.clone());
        directory(&to_cwd, request.start).map_err(to_wire)?;
        let same_profile = match &info.profile {
            None => profile.is_default(),
            Some(name) => profile.name() == name,
        };
        if request.from.node == self.node && same_profile && to_cwd == from_cwd {
            return Err(to_wire(TransferError::AlreadyThere));
        }
        let home = home(self.profiles, &profile).map_err(to_wire)?;
        let incoming = renamed(harness, &files, Some((&from_cwd, &to_cwd)))?;
        let placed = self
            .pull(source, transfer, &files, home, incoming, OnConflict::Refuse)
            .await?;
        let model = match &info.spec.mode {
            SessionMode::Structured { model, .. } => model.clone(),
            SessionMode::Terminal { .. } => None,
        };
        let named_profile = (!profile.is_default()).then(|| profile.name().clone());
        let mut spec = SessionSpec::new(
            harness,
            SessionMode::Structured {
                model,
                resume: Some(harness_session.clone()),
            },
        )
        .with_cwd(to_cwd.clone());
        spec.profile.clone_from(&named_profile);
        tracing::info!(%transfer, written = placed.written, same = placed.same, "session imported");
        Ok(SessionPlaced {
            imported: SessionImported {
                harness_session,
                profile: named_profile,
                cwd: to_cwd,
                written: placed.written,
                session: None,
            },
            spec,
            origin: SessionOrigin::Imported {
                node: request.from.node,
                session: request.from.session,
            },
        })
    }

    /// `memory.import` from `source`.
    pub(crate) async fn memory<S: Source>(
        &self,
        source: &S,
        request: &ImportMemory,
    ) -> Result<MemoryImported, WireError> {
        if let Some(project) = &request.project {
            if request.from.project.is_none() {
                return Err(to_wire(TransferError::ProjectWithoutSource));
            }
            directory(project, false).map_err(to_wire)?;
        }
        let _known = named(self.profiles, None, request.profile.as_ref()).map_err(to_wire)?;
        let manifest = source
            .export_memory(ExportMemory {
                profile: request.from.profile.clone(),
                project: request.from.project.clone(),
            })
            .await?;
        let transfer = manifest.transfer;
        let placed = self.place_memory(source, manifest, request).await;
        closed(source, transfer).await;
        placed
    }

    async fn place_memory<S: Source>(
        &self,
        source: &S,
        manifest: TransferManifest,
        request: &ImportMemory,
    ) -> Result<MemoryImported, WireError> {
        let TransferManifest {
            transfer,
            files,
            detail,
        } = manifest;
        let TransferDetail::Memory {
            harness,
            project: from_project,
            ..
        } = detail
        else {
            return Err(to_wire(TransferError::WrongKind("session")));
        };
        let profile =
            named(self.profiles, Some(harness), request.profile.as_ref()).map_err(to_wire)?;
        let home = home(self.profiles, &profile).map_err(to_wire)?;
        let projects = from_project
            .as_ref()
            .map(|from| (from, request.project.as_ref().unwrap_or(from)));
        let incoming = renamed(harness, &files, projects)?;
        let Placed {
            written,
            same,
            beside,
        } = self
            .pull(source, transfer, &files, home, incoming, OnConflict::Beside)
            .await?;
        tracing::info!(%transfer, written, same, beside = beside.len(), "memory imported");
        Ok(MemoryImported {
            written,
            same,
            beside,
        })
    }

    /// Fetch `files` of `transfer` from `source` into an incoming staging
    /// store and place them; the store is removed afterwards.
    async fn pull<S: Source>(
        &self,
        source: &S,
        transfer: TransferId,
        files: &BTreeMap<BundlePath, BundleFile>,
        home: PathBuf,
        incoming: Vec<Incoming>,
        on_conflict: OnConflict,
    ) -> Result<Placed, WireError> {
        check_limits(files).map_err(to_wire)?;
        let transfers = Arc::clone(self.transfers);
        let staged = tokio::task::spawn_blocking(move || transfers.stage(true))
            .await
            .map_err(|error| to_wire(TransferError::Task(error)))?
            .map_err(to_wire)?;
        let blobs = staged.blobs;
        let fetched = crate::fleet::fetch(files.values(), &blobs, |hash, offset| {
            source.read(ReadTransfer {
                transfer,
                hash,
                offset,
            })
        })
        .await;
        let placed = match fetched {
            Ok(()) => {
                let blobs = blobs.clone();
                tokio::task::spawn_blocking(move || {
                    vessel_fleet::place(&home, &incoming, &blobs, on_conflict)
                })
                .await
                .map_err(|error| to_wire(TransferError::Task(error)))
                .and_then(|placed| placed.map_err(|error| to_wire(TransferError::Place(error))))
            }
            Err(FetchError::Read(error)) => Err(error),
            Err(error) => Err(to_wire(TransferError::Fetch(error.to_string()))),
        };
        let root = blobs.root().to_owned();
        if let Err(error) = tokio::task::spawn_blocking(move || discard(&root)).await {
            tracing::warn!(%error, "could not remove an import's staging files");
        }
        placed
    }
}

/// `files` aimed at their places in the target home: renamed from the
/// first directory of `directories` to the second, or as they are.
fn renamed(
    harness: HarnessKind,
    files: &BTreeMap<BundlePath, BundleFile>,
    directories: Option<(&PathBuf, &PathBuf)>,
) -> Result<Vec<Incoming>, WireError> {
    files
        .iter()
        .map(|(bundle, file)| {
            let path = Path::new(bundle.as_str());
            let place = match directories {
                Some((from, to)) if from != to => rebase(harness, path, from, to)
                    .map_err(|error| to_wire(TransferError::Rebase(error)))?,
                Some(_) | None => path.to_owned(),
            };
            Ok(Incoming {
                bundle: bundle.clone(),
                place,
                file: file.clone(),
            })
        })
        .collect()
}

/// A working directory a session or memory can be imported for: absolute,
/// and a directory here when a session is to start in it.
fn directory(cwd: &Path, start: bool) -> Result<(), TransferError> {
    if !cwd.is_absolute() {
        return Err(TransferError::RelativeDirectory(cwd.to_owned()));
    }
    if start && !cwd.is_dir() {
        return Err(TransferError::NoDirectory(cwd.to_owned()));
    }
    Ok(())
}

/// Close the source's transfer; a failure only costs the source disk until
/// the transfer expires, so it is logged.
async fn closed<S: Source>(source: &S, transfer: TransferId) {
    if let Err(error) = source.close(CloseTransfer { transfer }).await {
        tracing::warn!(%transfer, %error, "could not close a transfer at its source; it will expire");
    }
}
