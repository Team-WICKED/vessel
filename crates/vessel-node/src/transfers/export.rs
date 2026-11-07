//! `session.export` and `memory.export`: which files (the harness's
//! knowledge), snapshot into a new transfer.
//!
//! [Decision Log]
//! - A session's files are the ones a move between profiles links
//!   (`SessionFiles::regular_files`), found in the home of the profile it
//!   runs (or ran) under, for the directory it runs in: its spec's, or the
//!   node's own working directory, as the runtime resolves it.
//! - A live session mid-turn is refused unless `force`, as for
//!   `session.move_profile`: the harness is still appending, so a copy
//!   would end in the middle of the turn. Forced, the snapshot takes the
//!   transcript as far as it is written (the turn goes on; nothing is
//!   cancelled, unlike a move, since the source keeps running). An ended
//!   session, live or stored, is exported as its files are.
//! - Terminal sessions are refused (`unsupported`): Vessel learns no
//!   harness session id for them, and resuming one is a structured
//!   session's `session/load`.
//! - `memory.export` without a profile means Claude Code's default
//!   profile: protocol 6 leaves the harness to the profile, and Claude
//!   Code is the harness whose memory is kept per project; a Codex
//!   profile's memory is asked for by naming the profile.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vessel_fleet::{Limits, Snapshot};
use vessel_harness::Profile;
use vessel_proto::{
    ExportMemory, ExportSession, HarnessKind, ProfileName, SessionMode, TransferDetail,
    TransferManifest,
};
use vessel_session::SessionError;

use super::{Holder, Staged, TransferError, Transfers, discard};
use crate::error::ServiceError;
use crate::profiles::NodeProfiles;
use crate::sessions::{Found, Sessions};

/// What exports need of the node.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Exporter<'a> {
    pub(crate) sessions: &'a Sessions,
    pub(crate) profiles: &'a NodeProfiles,
    pub(crate) transfers: &'a Arc<Transfers>,
}

impl Exporter<'_> {
    /// `session.export` for `holder` (whose ownership of the session the
    /// guard has checked).
    pub(crate) async fn session(
        &self,
        holder: Holder,
        request: ExportSession,
    ) -> Result<TransferManifest, ServiceError> {
        let info = match self.sessions.find(request.session).await? {
            Found::Live { handle, .. } => {
                let info = handle.info();
                if matches!(info.spec.mode, SessionMode::Terminal { .. }) {
                    return Err(TransferError::Terminal.into());
                }
                if info.status.is_live() && !request.force && handle.turn_running().await? {
                    return Err(TransferError::TurnInProgress.into());
                }
                info
            }
            Found::Stored(info) => info,
        };
        if matches!(info.spec.mode, SessionMode::Terminal { .. }) {
            return Err(TransferError::Terminal.into());
        }
        let session = info
            .harness_session
            .clone()
            .ok_or(TransferError::NotReady)?;
        let harness = info.spec.harness;
        let profile = match &info.profile {
            None => Profile::default_for(harness),
            Some(name) => self
                .profiles
                .get(name)
                .ok_or_else(|| ServiceError::ProfileGone(name.clone()))?,
        };
        let home = home(self.profiles, &profile)?;
        let cwd = info
            .spec
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .ok_or(SessionError::NoWorkingDirectory)?;
        let (staged, snapshot) = {
            let (transfers, cwd, session) =
                (Arc::clone(self.transfers), cwd.clone(), session.clone());
            tokio::task::spawn_blocking(move || {
                let files = vessel_harness::session_files(harness, &home, &cwd, &session)?
                    .regular_files(&home)?;
                take(&transfers, &home, &files)
            })
            .await
            .map_err(ServiceError::Task)??
        };
        let transfer = self
            .transfers
            .commit(staged, holder, snapshot.files.values())?;
        tracing::info!(%transfer, session = %request.session, files = snapshot.files.len(), "session exported");
        Ok(TransferManifest {
            transfer,
            files: snapshot.files,
            detail: TransferDetail::Session {
                harness_session: session,
                cwd,
                info: Box::new(info),
            },
        })
    }

    /// `memory.export`.
    pub(crate) async fn memory(
        &self,
        holder: Holder,
        request: ExportMemory,
    ) -> Result<TransferManifest, ServiceError> {
        let profile = named(self.profiles, None, request.profile.as_ref())?;
        if let Some(project) = &request.project
            && !project.is_absolute()
        {
            return Err(TransferError::RelativeDirectory(project.clone()).into());
        }
        let harness = profile.harness();
        let home = home(self.profiles, &profile)?;
        let (staged, snapshot) = {
            let (transfers, project) = (Arc::clone(self.transfers), request.project.clone());
            tokio::task::spawn_blocking(move || {
                let files = vessel_harness::memory_files(harness, &home, project.as_deref())?;
                take(&transfers, &home, &files)
            })
            .await
            .map_err(ServiceError::Task)??
        };
        let transfer = self
            .transfers
            .commit(staged, holder, snapshot.files.values())?;
        tracing::info!(%transfer, %harness, files = snapshot.files.len(), "memory exported");
        Ok(TransferManifest {
            transfer,
            files: snapshot.files,
            detail: TransferDetail::Memory {
                harness,
                profile: request.profile,
                project: request.project,
            },
        })
    }
}

/// Snapshot `files` of `home` into a new staging store; the store is
/// removed if that fails. Blocks.
fn take(
    transfers: &Transfers,
    home: &Path,
    files: &[PathBuf],
) -> Result<(Staged, Snapshot), TransferError> {
    let staged = transfers.stage(false)?;
    match vessel_fleet::snapshot(home, files, &staged.blobs, Limits::PROTOCOL) {
        Ok(snapshot) => {
            if !snapshot.skipped.is_empty() {
                tracing::info!(skipped = ?snapshot.skipped, "files left out of a transfer (gone, or no longer plain files)");
            }
            Ok((staged, snapshot))
        }
        Err(error) => {
            discard(staged.blobs.root());
            Err(error.into())
        }
    }
}

/// The profile `name` (Claude Code's default without one, or `harness`'s
/// when given), checked to be of `harness` when given.
pub(crate) fn named(
    profiles: &NodeProfiles,
    harness: Option<HarnessKind>,
    name: Option<&ProfileName>,
) -> Result<Profile, ServiceError> {
    let Some(name) = name else {
        return Ok(Profile::default_for(
            harness.unwrap_or(HarnessKind::ClaudeCode),
        ));
    };
    let profile = profiles
        .get(name)
        .ok_or_else(|| ServiceError::UnknownProfile(name.clone()))?;
    match harness {
        Some(harness) if profile.harness() != harness => Err(ServiceError::ProfileHarness {
            profile: name.clone(),
            profile_harness: profile.harness(),
            harness,
        }),
        Some(_) | None => Ok(profile),
    }
}

/// The harness home of `profile`, as the harness itself would resolve it
/// with the account the node was given (`NodeProfiles::home`, never the
/// process's environment).
pub(crate) fn home(profiles: &NodeProfiles, profile: &Profile) -> Result<PathBuf, ServiceError> {
    profiles
        .home(profile)
        .ok_or_else(|| ServiceError::HomeUnknown(profile.name().clone()))
}
