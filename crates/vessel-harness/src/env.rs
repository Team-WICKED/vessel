//! [`HarnessEnv`]: the account environment harness homes are resolved in,
//! given to the node rather than read from its process.
//!
//! [Decision Log]
//! - Problem: a profile that does not set its harness's home variable (the
//!   default profile, always) gets the one the node was started with, and
//!   failing that the harness's default under the user's home. Resolving
//!   those from the process meant every in-process node a test started
//!   resolved the default profile's home to the machine's real account
//!   (`~/.claude`), and code that writes into a profile's home (moving a
//!   session, applying the fleet's bundle, importing a session) wrote there.
//!   That happened once (a test of a session copy), and nothing but care
//!   kept it from happening again.
//! - Choice, fail-closed: the user's home and the harness home variables
//!   travel as a value. The binaries make it from their own environment
//!   (`HarnessEnv::from_vars(home, |name| std::env::var_os(name))` in
//!   `vesseld` and the CLI); a library never reads them from the process.
//!   There is no `Default`: whoever starts a node names its home, so a test
//!   that forgets cannot compile, and a test's value is a temporary
//!   directory with no variables ([`HarnessEnv::new`]).
//! - The same value shapes what a launched harness inherits: its home
//!   variable is set to this value's, or removed when this has none, and on
//!   Unix `HOME` is the user's home here, before the profile's own changes.
//!   For the binaries that is what the process had anyway; for a test it
//!   keeps even a fake's children out of the real account.
//! - Alternatives: an environment snapshot of every variable (more than
//!   homes need, and a value that holds tokens); a lint against
//!   `std::env` (catches calls, not a forgotten argument).
//! - Not here: `PATH` (`Lookup`, which tests replace), and the variables a
//!   harness's MCP servers must not clash with (checked against what the
//!   child really inherits, the process's).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use vessel_proc::CommandSpec;
use vessel_proto::HarnessKind;

use crate::harnesses::descriptor;
use crate::profile::Profile;

/// The user's home and the harness home variables (`CLAUDE_CONFIG_DIR`,
/// `CODEX_HOME`) harnesses run with on this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessEnv {
    user_home: PathBuf,
    vars: BTreeMap<&'static str, OsString>,
}

impl HarnessEnv {
    /// `user_home`, with none of the home variables set: every harness
    /// keeps its default home under `user_home`. What tests use, with a
    /// temporary directory.
    #[must_use]
    pub const fn new(user_home: PathBuf) -> Self {
        Self {
            user_home,
            vars: BTreeMap::new(),
        }
    }

    /// `user_home`, and the home variables `read` reports. Only the
    /// binaries pass a reader of their own environment.
    #[must_use]
    pub fn from_vars(user_home: PathBuf, read: impl Fn(&str) -> Option<OsString>) -> Self {
        let vars = Self::variables()
            .filter_map(|name| read(name).map(|value| (name, value)))
            .collect();
        Self { user_home, vars }
    }

    /// The variables this carries: every harness's home variable.
    pub fn variables() -> impl Iterator<Item = &'static str> {
        HarnessKind::ALL
            .into_iter()
            .filter_map(|kind| descriptor(kind).home.variable)
    }

    /// With `name` (one of [`variables`](Self::variables)) set to `value`;
    /// any other name is ignored.
    #[must_use]
    pub fn with_var(mut self, name: &str, value: OsString) -> Self {
        if let Some(name) = Self::variables().find(|known| *known == name) {
            let _previous = self.vars.insert(name, value);
        }
        self
    }

    /// The user's home.
    #[must_use]
    pub fn user_home(&self) -> &Path {
        &self.user_home
    }

    /// A home variable's value here.
    #[must_use]
    pub fn var(&self, name: &str) -> Option<&OsStr> {
        self.vars.get(name).map(OsString::as_os_str)
    }

    /// Where `profile`'s harness keeps its configuration, logins and
    /// sessions here; see [`crate::home`].
    #[must_use]
    pub fn home(&self, profile: &Profile) -> Option<PathBuf> {
        crate::home(profile, &self.user_home, |name| {
            self.var(name).map(ToOwned::to_owned)
        })
    }

    /// `command` for `harness`, inheriting this account and no other: its
    /// home variable as here (removed when unset here) and, on Unix, `HOME`.
    /// The profile's own changes come after.
    pub(crate) fn apply(&self, command: CommandSpec, harness: HarnessKind) -> CommandSpec {
        let command = match descriptor(harness).home.variable {
            Some(name) => match self.var(name) {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            },
            None => command,
        };
        if cfg!(unix) {
            command.env("HOME", &self.user_home)
        } else {
            command
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_home_variables_are_carried_and_homes_follow_them() {
        // Absolute on every system (`/x` is not, on Windows).
        let elsewhere = std::env::temp_dir().join("elsewhere-codex");
        let env = HarnessEnv::from_vars(PathBuf::from("/u"), |name| match name {
            "CODEX_HOME" => Some(elsewhere.clone().into_os_string()),
            _ => None,
        })
        .with_var("PATH", OsString::from("/bin"));
        assert_eq!(env.var("PATH"), None);
        assert_eq!(env.var("CLAUDE_CONFIG_DIR"), None);
        let codex = Profile::default_for(HarnessKind::Codex);
        assert_eq!(env.home(&codex), Some(elsewhere));
        let claude = Profile::default_for(HarnessKind::ClaudeCode);
        assert_eq!(env.home(&claude), Some(Path::new("/u").join(".claude")));
        assert!(HarnessEnv::variables().any(|name| name == "CLAUDE_CONFIG_DIR"));
    }
}
