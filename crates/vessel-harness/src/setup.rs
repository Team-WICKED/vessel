//! What first-run setup (`vessel init`, plan F5) needs to know about each
//! harness: how a person installs it, how they log in, where other
//! accounts' homes are, and how a profile for one is written into
//! `profiles.toml`.
//!
//! [Decision Log]
//! - Purpose: `vessel init` offers installs, finds the homes of the other
//!   accounts a person already uses, and tells them how to log in, without
//!   the CLI knowing any harness (`AGENTS.md` section 3).
//! - Installs: the vendor's documented `npm` command for each harness and
//!   adapter (plan decision 9). Vessel shows it and runs it only when the
//!   person says yes; it never picks an installer of its own.
//! - Logins: Vessel only checks (`login_check`) and prints the command for
//!   the person to run (`claude auth login`, `codex login`, `opencode auth
//!   login`), since logging in is interactive and belongs to them.
//! - Other accounts' homes: a directory beside the default home named
//!   `<default>-<name>` (`.claude-work` beside `.claude`), not a symbolic
//!   link, holding one of the harness's login files. Only names are looked
//!   at, never contents. Harnesses without a variable to move their home
//!   have no such homes.
//! - Writing a profile edits the file's TOML document in place
//!   (`toml_edit`), so a person's comments and layout stay as they were.

use std::io;
use std::path::{Path, PathBuf};

use vessel_proto::{HarnessKind, ProfileName};

use crate::harnesses::{Structured, descriptor};
use crate::lookup::Lookup;

/// What an install puts in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPart {
    /// The harness itself.
    Harness,
    /// Its ACP adapter, for structured sessions.
    Adapter,
}

/// A missing harness or adapter, and the vendor's command that installs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    /// Which harness.
    pub harness: HarnessKind,
    /// The harness or its adapter.
    pub part: InstallPart,
    /// The program the command needs (`npm`).
    pub program: &'static str,
    /// The whole command, as a person would type it.
    pub command: &'static str,
}

impl Install {
    /// The command's arguments after the program.
    #[must_use]
    pub fn args(&self) -> Vec<&'static str> {
        self.command.split_whitespace().skip(1).collect()
    }
}

/// What `lookup` lacks: each harness that is not installed, and each
/// adapter missing beside an installed harness.
#[must_use]
pub fn missing(lookup: &Lookup) -> Vec<Install> {
    let mut missing = Vec::new();
    for harness in HarnessKind::ALL {
        let descriptor = descriptor(harness);
        let (part, command) = if lookup.find(descriptor.binary).is_none() {
            (InstallPart::Harness, descriptor.install)
        } else {
            match &descriptor.structured {
                Structured::Adapter { binary, install } if lookup.find(binary).is_none() => {
                    (InstallPart::Adapter, *install)
                }
                Structured::Adapter { .. } | Structured::Native { .. } => continue,
            }
        };
        missing.push(Install {
            harness,
            part,
            program: command.split_whitespace().next().unwrap_or(command),
            command,
        });
    }
    missing
}

/// The command (binary and arguments) that logs a person in to `harness`;
/// `None` when that happens inside the harness's own interface.
#[must_use]
pub fn login_command(harness: HarnessKind) -> Option<(&'static str, &'static [&'static str])> {
    let descriptor = descriptor(harness);
    descriptor
        .login_command
        .map(|args| (descriptor.binary, args))
}

/// The variable that moves `harness`'s home, if it has one.
#[must_use]
pub fn home_variable(harness: HarnessKind) -> Option<&'static str> {
    descriptor(harness).home.variable
}

/// Another account's harness home, found beside the default one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountHome {
    /// Which harness.
    pub harness: HarnessKind,
    /// The directory.
    pub dir: PathBuf,
    /// The directory's name without its leading dot (`claude-work`): the
    /// profile name to propose, if it is a usable one.
    pub name: String,
}

impl AccountHome {
    /// The profile name to propose: `name`, unless it is not a valid
    /// profile name or names a harness (reserved for default profiles).
    #[must_use]
    pub fn profile_name(&self) -> Option<ProfileName> {
        if self.name.parse::<HarnessKind>().is_ok() {
            return None;
        }
        self.name.parse().ok()
    }
}

/// The homes of other accounts in `user_home`, by harness and name.
///
/// # Errors
/// `user_home` cannot be listed.
pub fn account_homes(user_home: &Path) -> io::Result<Vec<AccountHome>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(user_home)? {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(harness) = HarnessKind::ALL
            .into_iter()
            .find(|harness| is_account_dir_name(*harness, &file_name))
        else {
            continue;
        };
        let dir = entry.path();
        let is_dir = std::fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.is_dir());
        if !is_dir || !holds_a_login(harness, &dir) {
            continue;
        }
        found.push(AccountHome {
            harness,
            dir,
            name: file_name.trim_start_matches('.').to_owned(),
        });
    }
    found.sort_by(|a, b| (a.harness.name(), &a.dir).cmp(&(b.harness.name(), &b.dir)));
    Ok(found)
}

/// `.claude-<something>` for Claude Code: the default home's single name,
/// a dash, and more.
fn is_account_dir_name(harness: HarnessKind, file_name: &str) -> bool {
    let descriptor = descriptor(harness);
    let [default] = descriptor.home.default else {
        return false;
    };
    descriptor.home.variable.is_some()
        && !descriptor.account_files.is_empty()
        && file_name
            .strip_prefix(default)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|rest| !rest.is_empty())
}

fn holds_a_login(harness: HarnessKind, dir: &Path) -> bool {
    descriptor(harness)
        .account_files
        .iter()
        .any(|file| std::fs::symlink_metadata(dir.join(file)).is_ok())
}

/// Why a profile could not be added to `profiles.toml` text.
#[derive(Debug, thiserror::Error)]
pub enum AddProfileError {
    /// The text is not TOML (it would not load either); never quoted.
    #[error("profiles.toml is not valid TOML")]
    NotToml,
    /// `profiles` is there but is not a table.
    #[error("`profiles` in profiles.toml is not a table")]
    ProfilesNotATable,
    /// A profile of that name is defined already.
    #[error("a profile named `{0}` is defined already")]
    Taken(ProfileName),
}

/// `text` (the current `profiles.toml`, possibly empty) with a profile
/// `name` for `harness` that sets `variable` to `value` (its home). The
/// rest of the file stays as it was written.
///
/// # Errors
/// See [`AddProfileError`].
pub fn add_profile(
    text: &str,
    name: &ProfileName,
    harness: HarnessKind,
    variable: &str,
    value: &str,
) -> Result<String, AddProfileError> {
    let mut document: toml_edit::DocumentMut =
        text.parse().map_err(|_error| AddProfileError::NotToml)?;
    let profiles = document
        .entry("profiles")
        .or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        })
        .as_table_mut()
        .ok_or(AddProfileError::ProfilesNotATable)?;
    if profiles
        .iter()
        .any(|(defined, _)| defined.eq_ignore_ascii_case(name.as_str()))
    {
        return Err(AddProfileError::Taken(name.clone()));
    }
    let mut env = toml_edit::InlineTable::new();
    let _none = env.insert(variable, value.into());
    let mut profile = toml_edit::Table::new();
    let _none = profile.insert("harness", toml_edit::value(descriptor(harness).profile));
    let _none = profile.insert("env", toml_edit::value(env));
    let _none = profiles.insert(name.as_str(), toml_edit::Item::Table(profile));
    Ok(document.to_string())
}

#[cfg(test)]
mod tests;
