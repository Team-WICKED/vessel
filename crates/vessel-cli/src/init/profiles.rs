//! Profiles for other accounts' harness homes (`~/.claude-work` beside
//! `~/.claude`), written into `profiles.toml` after a yes, and a login
//! check for every profile.
//!
//! Only directory and file names are looked at, never contents, and only
//! a profile's home is ever printed, never another of its values (they
//! may be secrets).

use std::fmt::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use vessel_harness::{
    AccountHome, InstallPart, Profile, Profiles, account_homes, add_profile, home, home_variable,
    login_check, login_command, missing, parse_profiles,
};
use vessel_proto::{HarnessKind, ProfileName};

use super::{Init, Outcome, shown};
use crate::error::CliError;

/// The longest a login check may take; one that does not answer counts as
/// not logged in (the node's move uses the same limit).
const LOGIN_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) async fn run(init: &mut Init) -> Result<(), CliError> {
    let home_dir = init.options.home.clone();
    let text = init
        .data_dir
        .read_profiles()
        .map_err(CliError::Prepare)?
        .map(|file| file.text)
        .unwrap_or_default();
    let path = init.data_dir.profiles_path();
    let profiles = match parse_profiles(&text, Some(&home_dir)) {
        Ok(profiles) => profiles,
        Err(error) => {
            let detail = format!(
                "{} does not load ({error}); fix it and run again",
                shown(init, &path)
            );
            init.report(Outcome::Skipped, "profiles", &detail).await?;
            return Ok(());
        }
    };
    let accounts = match account_homes(&home_dir) {
        Ok(accounts) => accounts,
        Err(error) => {
            let detail = format!("cannot list {}: {error}", shown(init, &home_dir));
            init.report(Outcome::Skipped, "profiles", &detail).await?;
            Vec::new()
        }
    };

    let mut proposals = Vec::new();
    for account in &accounts {
        let shown_dir = shown(init, &account.dir);
        let existing = profiles.defined().find(|profile| {
            profile.harness() == account.harness
                && home(profile, &home_dir, |name| std::env::var_os(name)).as_deref()
                    == Some(account.dir.as_path())
        });
        if let Some(profile) = existing {
            let step = format!("profile {}", profile.name());
            init.report(Outcome::Already, &step, &shown_dir).await?;
            continue;
        }
        let Some(name) = account.profile_name() else {
            let step = format!("profile for {shown_dir}");
            init.report(
                Outcome::Skipped,
                &step,
                "its name is not usable as a profile name; add one by hand",
            )
            .await?;
            continue;
        };
        if profiles.get(&name).is_some() {
            let step = format!("profile {name}");
            init.report(
                Outcome::Skipped,
                &step,
                &format!("a profile of that name has another home than {shown_dir}"),
            )
            .await?;
            continue;
        }
        proposals.push((name, account));
    }

    let profiles = if proposals.is_empty() {
        profiles
    } else {
        propose(init, &text, &proposals, profiles).await?
    };
    check_logins(init, &profiles).await
}

/// Show the proposed profiles and write them after a yes; returns the
/// profiles in effect afterwards.
async fn propose(
    init: &mut Init,
    text: &str,
    proposals: &[(ProfileName, &AccountHome)],
    current: Profiles,
) -> Result<Profiles, CliError> {
    let path = init.data_dir.profiles_path();
    let mut list = String::new();
    for (name, account) in proposals {
        let _infallible = writeln!(
            list,
            "  {name}: {} in {}",
            account.harness,
            shown(init, &account.dir)
        );
    }
    crate::io::note(&format!("Profiles for the accounts found:\n{list}")).await?;
    let question = format!("Add them to {}?", shown(init, &path));
    if !init.confirm(&question).await? {
        for (name, _account) in proposals {
            init.report(
                Outcome::Skipped,
                &format!("profile {name}"),
                "not added (you said no)",
            )
            .await?;
        }
        return Ok(current);
    }
    let mut updated = text.to_owned();
    for (name, account) in proposals {
        let Some(variable) = home_variable(account.harness) else {
            continue;
        };
        let value = home_value(&account.dir, &init.options.home);
        updated = add_profile(&updated, name, account.harness, variable, &value)
            .map_err(CliError::AddProfile)?;
    }
    // Checked before writing: a node must be able to load the result.
    let profiles = parse_profiles(&updated, Some(&init.options.home))
        .map_err(|error| CliError::ProfilesAfterAdd(error.to_string()))?;
    init.data_dir
        .write_profiles(&updated)
        .map_err(CliError::Prepare)?;
    for (name, account) in proposals {
        init.report(
            Outcome::Done,
            &format!("profile {name}"),
            &format!("added for {}", shown(init, &account.dir)),
        )
        .await?;
    }
    init.todo.push(
        "a node that is running reads new profiles with `vessel profiles --reload`".to_owned(),
    );
    Ok(profiles)
}

/// `~/<name>` for a directory right under the home, so the file stays
/// right if the home moves; the full path otherwise.
fn home_value(dir: &Path, home_dir: &Path) -> String {
    match dir.strip_prefix(home_dir) {
        Ok(rest) if rest.components().count() == 1 => format!("~/{}", rest.display()),
        Ok(_) | Err(_) => dir.display().to_string(),
    }
}

/// Run each installed harness's login check, per profile; print how to log
/// in where one fails. Never logs in itself.
async fn check_logins(init: &mut Init, profiles: &Profiles) -> Result<(), CliError> {
    // Harnesses that are not installed were reported already.
    let absent: Vec<HarnessKind> = missing(&init.lookup)
        .into_iter()
        .filter(|install| install.part == InstallPart::Harness)
        .map(|install| install.harness)
        .collect();
    for profile in profiles.all() {
        if absent.contains(&profile.harness()) {
            continue;
        }
        let step = format!("login {}", profile.name());
        let check = match login_check(&init.lookup, profile) {
            Ok(Some(check)) => check,
            Ok(None) => {
                init.report(
                    Outcome::Skipped,
                    &step,
                    "no login check for this harness; log in from its own interface",
                )
                .await?;
                continue;
            }
            Err(error) => {
                init.report(Outcome::Skipped, &step, &error.to_string())
                    .await?;
                continue;
            }
        };
        let mut command = tokio::process::Command::from(check.to_command());
        let _configured = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let logged_in = match command.spawn() {
            Ok(mut child) => matches!(
                tokio::time::timeout(LOGIN_CHECK_TIMEOUT, child.wait()).await,
                Ok(Ok(status)) if status.success()
            ),
            Err(_) => false,
        };
        if logged_in {
            init.report(Outcome::Already, &step, "logged in").await?;
            continue;
        }
        init.report(Outcome::Skipped, &step, "not logged in")
            .await?;
        if let Some(hint) = login_hint(init, profile) {
            init.todo
                .push(format!("log in to {}: `{hint}`", profile.name()));
        }
    }
    Ok(())
}

/// The command a person runs to log `profile` in: the harness's login
/// command, with the profile's home variable in front when it sets one
/// (the only value of a profile ever shown).
fn login_hint(init: &Init, profile: &Profile) -> Option<String> {
    let (binary, args) = login_command(profile.harness())?;
    let command = std::iter::once(binary)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let home_set = home_variable(profile.harness())
        .and_then(|variable| profile.get(&variable.parse().ok()?).map(|_value| variable));
    let Some(variable) = home_set else {
        return Some(command);
    };
    let dir = home(profile, &init.options.home, |name| std::env::var_os(name))?;
    Some(if cfg!(windows) {
        format!("$env:{variable}='{}'; {command}", dir.display())
    } else {
        format!("{variable}='{}' {command}", dir.display())
    })
}
