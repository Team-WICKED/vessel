//! `session.move_profile` (ADR 0011, H2): what the node decides before the
//! session runtime moves a session. Which profile, which homes, and whether
//! the target account is logged in.
//!
//! [Decision Log]
//! - The target must be logged in: the harness's own check
//!   (`vessel_harness::login_check`, `claude auth status`) run with the
//!   target profile's environment, so it checks that account. A harness
//!   without a check is trusted (the restart then fails if it is not, and
//!   the session goes back to its previous agent). The check is bounded by
//!   [`LOGIN_CHECK_TIMEOUT`]: a check that hangs (asking for input nobody
//!   gives) counts as not logged in.
//! - The homes come from `NodeProfiles::home`: the account the node was
//!   given (`HarnessEnv`), as the harness itself would resolve them there.

use std::process::Stdio;
use std::time::Duration;

use vessel_harness::{HarnessEnv, Lookup, Profile, login_check};
use vessel_proto::{MoveProfile, SessionInfo, SessionMode};
use vessel_session::{MoveError, ProfileMove, SessionError};

use super::NodeProfiles;
use super::launch::Launcher;
use crate::error::ServiceError;

/// The longest a login check may take.
pub(crate) const LOGIN_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// What the runtime needs to move `info`'s session as `request` asks.
///
/// # Errors
/// An unknown target profile or one of another harness, a current profile
/// no longer defined, a home that cannot be known, a target that is not
/// logged in.
pub(crate) async fn plan(
    profiles: &NodeProfiles,
    launcher: &Launcher,
    lookup: &Lookup,
    info: &SessionInfo,
    request: &MoveProfile,
) -> Result<ProfileMove, ServiceError> {
    if matches!(info.spec.mode, SessionMode::Terminal { .. }) {
        // Refused before any check runs a harness.
        return Err(SessionError::Move(MoveError::Terminal).into());
    }
    let harness = info.spec.harness;
    let target = match &request.profile {
        None => Profile::default_for(harness),
        Some(name) => profiles
            .get(name)
            .ok_or_else(|| ServiceError::UnknownProfile(name.clone()))?,
    };
    if target.harness() != harness {
        return Err(ServiceError::ProfileHarness {
            profile: target.name().clone(),
            profile_harness: target.harness(),
            harness,
        });
    }
    let current = match &info.profile {
        None => Profile::default_for(harness),
        Some(name) => profiles
            .get(name)
            .ok_or_else(|| ServiceError::ProfileGone(name.clone()))?,
    };
    let from_home = profiles
        .home(&current)
        .ok_or_else(|| ServiceError::HomeUnknown(current.name().clone()))?;
    let to_home = profiles
        .home(&target)
        .ok_or_else(|| ServiceError::HomeUnknown(target.name().clone()))?;
    logged_in(lookup, profiles.env(), &target).await?;
    let (mcp_servers, notices) = launcher.mcp_servers(&info.spec);
    Ok(ProfileMove {
        mcp_servers,
        notices,
        profile: target,
        from_home,
        to_home,
        force: request.force,
    })
}

/// Run the target's login check; refuse unless it exits 0.
async fn logged_in(
    lookup: &Lookup,
    env: &HarnessEnv,
    target: &Profile,
) -> Result<(), ServiceError> {
    let lookup = lookup.clone();
    let env = env.clone();
    let profile = target.clone();
    let check = tokio::task::spawn_blocking(move || login_check(&lookup, &env, &profile))
        .await
        .map_err(ServiceError::Task)?
        .map_err(SessionError::Launch)?;
    let Some(check) = check else {
        return Ok(());
    };
    let mut command = tokio::process::Command::from(check.to_command());
    let _configured = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let not_logged_in = || ServiceError::NotLoggedIn(target.name().clone());
    let mut child = command.spawn().map_err(|_error| not_logged_in())?;
    match tokio::time::timeout(LOGIN_CHECK_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(_) | Err(_)) | Err(_) => Err(not_logged_in()),
    }
}
