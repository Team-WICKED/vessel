//! [`home`] and [`login_check`]: which account a profile runs as.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use vessel_proc::CommandSpec;

use crate::error::HarnessError;
use crate::harnesses::descriptor;
use crate::launch::with_profile;
use crate::lookup::Lookup;
use crate::profile::Profile;

/// Where `profile`'s harness keeps its configuration, logins and sessions
/// (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, or the harness's default under
/// `user_home`).
///
/// `inherited` reads the node's own environment (normally
/// `|name| std::env::var_os(name)`): a profile that neither sets nor
/// removes the home variable, the default profile included, gets whatever
/// the node was started with, as the harness would.
///
/// `None` when the home cannot be known: the variable holds a relative
/// path, which the harness would resolve against each session's working
/// directory.
#[must_use]
pub fn home(
    profile: &Profile,
    user_home: &Path,
    inherited: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let home = &descriptor(profile.harness()).home;
    let chosen = home.variable.and_then(|variable| {
        if let Some(value) = profile.get(variable) {
            Some(PathBuf::from(value))
        } else if profile.unsets(variable) {
            None
        } else {
            inherited(variable).map(PathBuf::from)
        }
    });
    match chosen {
        // An empty value counts as unset, as it does for a path.
        Some(path) if path.as_os_str().is_empty() => Some(default(user_home, home.default)),
        Some(path) if path.is_absolute() => Some(path),
        Some(_) => None,
        None => Some(default(user_home, home.default)),
    }
}

fn default(user_home: &Path, parts: &[&str]) -> PathBuf {
    parts
        .iter()
        .fold(user_home.to_owned(), |path, part| path.join(part))
}

/// The command that reports whether `profile`'s harness is logged in,
/// without changing anything (`claude auth status`, `codex login status`),
/// run with the profile's environment so it checks that profile's account;
/// `None` if the harness has no such command.
///
/// The commands were confirmed from `--help` only. Whether a failed check
/// exits non-zero was not tried (that would touch an account), so callers
/// should treat a non-zero exit as "not logged in" and a zero exit as
/// "probably logged in".
///
/// # Errors
/// [`HarnessError::NotInstalled`] if the harness is missing.
pub fn login_check(
    lookup: &Lookup,
    profile: &Profile,
) -> Result<Option<CommandSpec>, HarnessError> {
    let harness = profile.harness();
    let descriptor = descriptor(harness);
    let Some(args) = descriptor.login else {
        return Ok(None);
    };
    let program = lookup
        .find(descriptor.binary)
        .ok_or(HarnessError::NotInstalled {
            harness,
            binary: descriptor.binary,
        })?;
    Ok(Some(with_profile(
        CommandSpec::new(program).args(args.iter()),
        profile,
    )))
}

#[cfg(test)]
mod tests {
    use vessel_proto::HarnessKind;

    use super::*;
    use crate::profile::parse_profiles;

    fn profile(text: &str, name: &str) -> Profile {
        parse_profiles(text, Some(Path::new("/home/u")))
            .unwrap()
            .get(&name.parse().unwrap())
            .unwrap()
    }

    #[test]
    fn homes_follow_the_profile_then_the_node() {
        let user = Path::new("/home/u");
        let nothing = |_: &str| None;
        let claude = Profile::default_for(HarnessKind::ClaudeCode);
        assert_eq!(home(&claude, user, nothing), Some(user.join(".claude")));
        let inherited =
            |name: &str| (name == "CLAUDE_CONFIG_DIR").then(|| OsString::from("/srv/c"));
        assert_eq!(
            home(&claude, user, inherited),
            Some(PathBuf::from("/srv/c"))
        );
        let relative = |_: &str| Some(OsString::from("rel"));
        assert_eq!(home(&claude, user, relative), None);
        let empty = |_: &str| Some(OsString::new());
        assert_eq!(home(&claude, user, empty), Some(user.join(".claude")));

        let alt = profile(
            "[profiles.alt]\nharness = \"claude\"\nenv = { CLAUDE_CONFIG_DIR = \"~/.claude-alt\" }\n",
            "alt",
        );
        assert_eq!(home(&alt, user, inherited), Some(user.join(".claude-alt")));
        let reset = profile(
            "[profiles.reset]\nharness = \"claude\"\nunset = [\"CLAUDE_CONFIG_DIR\"]\n",
            "reset",
        );
        assert_eq!(home(&reset, user, inherited), Some(user.join(".claude")));

        let codex = Profile::default_for(HarnessKind::Codex);
        assert_eq!(home(&codex, user, nothing), Some(user.join(".codex")));
        let opencode = Profile::default_for(HarnessKind::OpenCode);
        assert_eq!(
            home(&opencode, user, nothing),
            Some(user.join(".config").join("opencode"))
        );
        let gemini = Profile::default_for(HarnessKind::Gemini);
        assert_eq!(home(&gemini, user, relative), Some(user.join(".gemini")));
    }

    #[test]
    fn login_checks_carry_the_profile() {
        let dir = tempfile_dir();
        let lookup = Lookup::new([dir.clone()]);
        let alt = profile(
            "[profiles.alt]\nharness = \"claude\"\nenv = { CLAUDE_CONFIG_DIR = \"/a\" }\nunset = [\"ANTHROPIC_API_KEY\"]\n",
            "alt",
        );
        assert_eq!(
            login_check(&lookup, &alt),
            Err(HarnessError::NotInstalled {
                harness: HarnessKind::ClaudeCode,
                binary: "claude"
            })
        );
        let gemini = Profile::default_for(HarnessKind::Gemini);
        assert_eq!(login_check(&lookup, &gemini), Ok(None));
        drop(std::fs::remove_dir(&dir));
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vessel-home-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
