//! Harness profiles (ADR 0011): one harness, several accounts or
//! configurations, chosen per session.
//!
//! A [`Profile`] extends one harness with environment variables to set, to
//! remove, and extra terminal arguments. Every harness has an implicit
//! default profile named after its command (`claude`, `codex`, `gemini`,
//! `opencode`), which is the node's environment as is: launching with it is
//! exactly the launch from before profiles. The node defines the others in
//! `profiles.toml`, read by [`parse_profiles`]:
//!
//! ```toml
//! [profiles.claude-alt]
//! harness = "claude"                 # the harness it extends
//! label = "Claude (Alt)"
//! env = { CLAUDE_CONFIG_DIR = "~/.claude-alt" }
//! unset = ["ANTHROPIC_API_KEY"]      # removed from the inherited environment
//! args = ["--model", "opus"]         # terminal mode only, before the session's own
//!
//! [profiles.claude-qwen]
//! harness = "claude"
//! model = "qwen-coder"               # a route of `models.toml` (ADR 0015)
//! ```
//!
//! A profile with `model` runs its harness against the node's model proxy,
//! pinned to the route's models (`routing`); only harnesses that can be
//! pointed there may have one, and such a profile may not set the
//! variables the route sets or removes.
//!
//! Values may be secrets (an API key in `env`): `Debug` and
//! [`Profile::info`] show variable names only, and parse errors never quote
//! the file.
//!
//! [Decision Log]
//! - Purpose: a pure parser, so the node owns reading the file (path,
//!   permissions, reload) and this crate owns what the text means.
//! - Format: TOML through the workspace's `toml_edit` (its `serde` feature
//!   reads the file into typed structs that refuse unknown keys), so there
//!   is one TOML implementation in the workspace.
//! - Errors: the TOML library's message with line and column, but not its
//!   source excerpt, which would quote the line, and a line of `env` may
//!   hold a secret that `profile.reload` would then send to a client. The
//!   checks after typing name the profile and the key.
//! - Variable names are compared ignoring ASCII case (two `env` entries,
//!   two `unset` entries, or one in both): Windows treats them as one
//!   variable, and a file that means different things on different
//!   systems is worse than refusing `Foo` beside `FOO` on Unix.
//! - A profile name that parses as a harness (`claude`, `Claude-Code`, ...)
//!   is reserved, so `--profile claude` always means the default profile.
//!   Duplicate names are refused ignoring case: clients and people type
//!   them, and two profiles one letter-case apart are a mistake.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use serde::Deserialize;
use vessel_proto::{EnvName, HarnessKind, InvalidName, ModelRouteName, ProfileInfo, ProfileName};

use crate::harnesses::descriptor;
use crate::routing::{overriding, routed_variables};

/// A harness with environment changes and terminal arguments.
#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    name: ProfileName,
    harness: HarnessKind,
    label: Option<String>,
    env: BTreeMap<EnvName, String>,
    unset: BTreeSet<EnvName>,
    args: Vec<String>,
    model: Option<ModelRouteName>,
}

impl Profile {
    /// The implicit default profile of `harness`: no changes, so launching
    /// with it is the launch from before profiles. Its name is the harness's
    /// command (`claude` for Claude Code).
    #[must_use]
    pub fn default_for(harness: HarnessKind) -> Self {
        Self {
            name: default_name(harness),
            harness,
            label: None,
            env: BTreeMap::new(),
            unset: BTreeSet::new(),
            args: Vec::new(),
            model: None,
        }
    }

    /// Its name.
    #[must_use]
    pub const fn name(&self) -> &ProfileName {
        &self.name
    }

    /// The harness it extends.
    #[must_use]
    pub const fn harness(&self) -> HarnessKind {
        self.harness
    }

    /// Its label for people, if any.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// Whether this is a harness's implicit default profile.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.name == default_name(self.harness)
    }

    /// The variables it sets, with their values (`~/` already expanded).
    /// The values may be secrets: they go into the harness's environment
    /// and nowhere else.
    pub fn env(&self) -> impl Iterator<Item = (&EnvName, &str)> {
        self.env.iter().map(|(name, value)| (name, value.as_str()))
    }

    /// The value it gives `name`, if it sets it. Names compare as this
    /// system's environment does ([`same_variable`]).
    #[must_use]
    pub fn get(&self, name: &EnvName) -> Option<&str> {
        self.env
            .iter()
            .find(|(key, _)| same_variable(key.as_str(), name.as_str()))
            .map(|(_, value)| value.as_str())
    }

    /// The inherited variables it removes.
    pub fn unset(&self) -> impl Iterator<Item = &EnvName> {
        self.unset.iter()
    }

    /// Whether it removes `name`. Names compare as this system's
    /// environment does ([`same_variable`]).
    #[must_use]
    pub fn unsets(&self, name: &EnvName) -> bool {
        self.unset
            .iter()
            .any(|key| same_variable(key.as_str(), name.as_str()))
    }

    /// Arguments added before the session's own, in terminal mode only.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The model route its sessions are pointed at through the node's
    /// proxy (ADR 0015), if any.
    #[must_use]
    pub const fn model(&self) -> Option<&ModelRouteName> {
        self.model.as_ref()
    }

    /// What a node reports about it: names, never values.
    #[must_use]
    pub fn info(&self) -> ProfileInfo {
        ProfileInfo {
            name: self.name.clone(),
            label: self.label.clone(),
            harness: self.harness,
            sets: self.env.keys().cloned().collect(),
            model: self.model.clone(),
        }
    }
}

/// Names only: values may be secrets.
impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("name", &self.name)
            .field("harness", &self.harness)
            .field("label", &self.label)
            .field("sets", &self.env.keys().collect::<Vec<_>>())
            .field("unset", &self.unset)
            .field("args", &self.args)
            .field("model", &self.model)
            .finish()
    }
}

/// Whether two variable names are one variable on this system: ignoring
/// ASCII case on Windows, as its environment does, and exactly elsewhere.
/// Every lookup of a profile's variables (its home, its removals, the MCP
/// check) goes through this, so they agree with what the harness gets.
/// Parsing is stricter on every system (two names one case apart are
/// refused), so a file never means different things on two systems.
#[must_use]
pub fn same_variable(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// The name of `harness`'s default profile.
#[expect(
    clippy::expect_used,
    reason = "the four names are constants, and a test parses each of them"
)]
fn default_name(harness: HarnessKind) -> ProfileName {
    descriptor(harness)
        .profile
        .parse()
        .expect("a default profile name follows the profile name rule")
}

/// The profiles of one node: every harness's default and those defined in
/// `profiles.toml`. The defaults are built once, with the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profiles {
    defaults: Vec<Profile>,
    defined: BTreeMap<ProfileName, Profile>,
}

/// The default profiles only (no `profiles.toml`).
impl Default for Profiles {
    fn default() -> Self {
        Self::with(BTreeMap::new())
    }
}

impl Profiles {
    fn with(defined: BTreeMap<ProfileName, Profile>) -> Self {
        Self {
            defaults: HarnessKind::ALL
                .into_iter()
                .map(Profile::default_for)
                .collect(),
            defined,
        }
    }

    /// The profile named `name`: a defined one, or a harness's default
    /// (names do not overlap: a defined one may not take a harness's name).
    #[must_use]
    pub fn get(&self, name: &ProfileName) -> Option<&Profile> {
        self.defined
            .get(name)
            .or_else(|| self.defaults.iter().find(|profile| profile.name() == name))
    }

    /// Every profile: the defaults (in harness order), then the defined
    /// ones by name.
    pub fn all(&self) -> impl Iterator<Item = &Profile> {
        self.defaults.iter().chain(self.defined.values())
    }

    /// The profiles defined in the file, by name.
    pub fn defined(&self) -> impl Iterator<Item = &Profile> {
        self.defined.values()
    }
}

/// `profiles.toml` as typed: the file is `[profiles.<name>]` tables and
/// nothing else.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    profiles: BTreeMap<String, RawProfile>,
}

/// One `[profiles.<name>]` table. Names are checked after typing, so the
/// errors can name the profile and the key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    harness: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    unset: Vec<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    model: Option<String>,
}

/// `profiles.toml` could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfilesError {
    /// Not TOML, or not the shape of `profiles.toml` (an unknown key, a
    /// missing `harness`, a value of the wrong type). The message names the
    /// key and the position, and never quotes the file.
    #[error("profiles.toml, {}: {message}", position.map_or_else(|| "unknown position".to_owned(), |at| at.to_string()))]
    Toml {
        /// Where, if the TOML library said.
        position: Option<Position>,
        /// The TOML library's message.
        message: String,
    },
    /// One profile is wrong.
    #[error("profiles.toml, profile `{profile}`: {problem}")]
    Profile {
        /// The profile's name as written.
        profile: String,
        /// What is wrong with it.
        problem: ProfileProblem,
    },
}

/// What is wrong with one profile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileProblem {
    /// The name breaks the profile name rule.
    #[error("{0}")]
    InvalidName(InvalidName),
    /// The name is a harness's, so it is its default profile's.
    #[error("the name is reserved for the {harness} harness's default profile")]
    Reserved {
        /// The harness the name means.
        harness: HarnessKind,
    },
    /// Another profile has the same name ignoring case.
    #[error("profile `{other}` has the same name (profile names are compared ignoring case)")]
    Duplicate {
        /// The other profile, as written.
        other: String,
    },
    /// `harness` names no harness.
    #[error("`harness`: unknown harness `{value}` (one of claude, codex, gemini, opencode)")]
    UnknownHarness {
        /// The value given.
        value: String,
    },
    /// A name in `env` or `unset` is not a variable name.
    #[error("`{key}`: `{name}` is not a valid environment variable name ({})", vessel_proto::NameKind::EnvVar.rule())]
    InvalidVariable {
        /// `env` or `unset`.
        key: &'static str,
        /// The name given.
        name: String,
    },
    /// A variable is listed twice under one key (ignoring case).
    #[error("`{key}`: `{name}` is listed twice (variable names are compared ignoring case)")]
    RepeatedVariable {
        /// `env` or `unset`.
        key: &'static str,
        /// The second spelling.
        name: EnvName,
    },
    /// A variable is both set and removed.
    #[error("`{name}` is both in `env` and in `unset`")]
    SetAndUnset {
        /// The variable.
        name: EnvName,
    },
    /// A value starts with `~` but the node user's home is unknown or is
    /// not text.
    #[error(
        "`{key}`: `~` needs the node user's home directory, which is unknown or not valid text"
    )]
    NoHome {
        /// `env.<NAME>` or `args`.
        key: String,
    },
    /// `args` on a profile whose harness takes no terminal arguments.
    #[error("`args`: {harness} takes no extra terminal arguments")]
    ArgsUnsupported {
        /// The harness.
        harness: HarnessKind,
    },
    /// `model` is not a route name.
    #[error("`model`: {0}")]
    InvalidRoute(InvalidName),
    /// `model` on a profile whose harness cannot be pointed at the node's
    /// model proxy.
    #[error("`model`: {harness} cannot use a model route yet (Claude Code and Codex can)")]
    NotRoutable {
        /// The harness.
        harness: HarnessKind,
    },
    /// A routed profile sets a variable its route sets or removes.
    #[error("`env.{name}`: the profile's model route sets this variable")]
    RoutedVariable {
        /// The variable.
        name: EnvName,
    },
    /// A routed profile's argument would choose another model, provider or
    /// configuration profile.
    #[error("`args`: `{argument}` would take the profile's sessions off its model route")]
    RoutedArgument {
        /// The argument.
        argument: String,
    },
}

/// Read `profiles.toml` from its text. `user_home` is what `~/` expands
/// to; `None` if the node user has none (a value using `~` is then an
/// error).
///
/// Pure: the caller reads the file (and checks it is owner-only).
///
/// # Errors
/// The first problem found, naming the profile and key; see
/// [`ProfilesError`].
pub fn parse_profiles(text: &str, user_home: Option<&Path>) -> Result<Profiles, ProfilesError> {
    let raw: RawFile = toml_edit::de::from_str(text).map_err(|error| ProfilesError::Toml {
        position: position(text, error.span()),
        message: without_values(error.message()),
    })?;
    let home = user_home.and_then(Path::to_str);
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut defined = BTreeMap::new();
    for (written, raw_profile) in raw.profiles {
        let fail = |problem| ProfilesError::Profile {
            profile: written.clone(),
            problem,
        };
        let name: ProfileName = written
            .parse()
            .map_err(|error| fail(ProfileProblem::InvalidName(error)))?;
        if let Ok(harness) = written.parse::<HarnessKind>() {
            return Err(fail(ProfileProblem::Reserved { harness }));
        }
        if let Some(other) = seen.insert(written.to_ascii_lowercase(), written.clone()) {
            return Err(fail(ProfileProblem::Duplicate { other }));
        }
        let profile = profile(name, raw_profile, home).map_err(fail)?;
        let _new = defined.insert(profile.name.clone(), profile);
    }
    Ok(Profiles::with(defined))
}

/// One profile's checks, after its name.
fn profile(
    name: ProfileName,
    raw: RawProfile,
    home: Option<&str>,
) -> Result<Profile, ProfileProblem> {
    let harness =
        raw.harness
            .parse::<HarnessKind>()
            .map_err(|_unknown| ProfileProblem::UnknownHarness {
                value: raw.harness.clone(),
            })?;
    if !raw.args.is_empty() && !descriptor(harness).terminal_args {
        return Err(ProfileProblem::ArgsUnsupported { harness });
    }
    let model = raw
        .model
        .as_deref()
        .map(str::parse::<ModelRouteName>)
        .transpose()
        .map_err(ProfileProblem::InvalidRoute)?;
    let routed = match model {
        Some(_) if descriptor(harness).model_proxy.is_none() => {
            return Err(ProfileProblem::NotRoutable { harness });
        }
        Some(_) => routed_variables(harness),
        None => Vec::new(),
    };
    let mut set_folded = BTreeSet::new();
    let mut env = BTreeMap::new();
    for (written, value) in raw.env {
        let variable = variable("env", &written)?;
        if !set_folded.insert(written.to_ascii_uppercase()) {
            return Err(ProfileProblem::RepeatedVariable {
                key: "env",
                name: variable,
            });
        }
        if routed.iter().any(|name| written.eq_ignore_ascii_case(name)) {
            return Err(ProfileProblem::RoutedVariable { name: variable });
        }
        let value = expand(&value, home).ok_or_else(|| ProfileProblem::NoHome {
            key: format!("env.{written}"),
        })?;
        let _new = env.insert(variable, value);
    }
    let mut unset_folded = BTreeSet::new();
    let mut unset = BTreeSet::new();
    for written in raw.unset {
        let variable = variable("unset", &written)?;
        let folded = written.to_ascii_uppercase();
        if set_folded.contains(&folded) {
            return Err(ProfileProblem::SetAndUnset { name: variable });
        }
        if !unset_folded.insert(folded) {
            return Err(ProfileProblem::RepeatedVariable {
                key: "unset",
                name: variable,
            });
        }
        let _new = unset.insert(variable);
    }
    let args = raw
        .args
        .iter()
        .map(|arg| expand(arg, home))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| ProfileProblem::NoHome {
            key: "args".to_owned(),
        })?;
    if model.is_some()
        && let Some(argument) = overriding(harness, &args)
    {
        return Err(ProfileProblem::RoutedArgument { argument });
    }
    Ok(Profile {
        name,
        harness,
        label: raw.label,
        env,
        unset,
        args,
        model,
    })
}

fn variable(key: &'static str, written: &str) -> Result<EnvName, ProfileProblem> {
    written
        .parse()
        .map_err(|_invalid| ProfileProblem::InvalidVariable {
            key,
            name: written.to_owned(),
        })
}

/// `~` and a leading `~/` become the home; anything else is kept as text
/// (`~user` and a `~` later in the value too, as in a shell). `None` when
/// the value needs the home and there is none.
///
/// The path after `~/` is written with the system's separator, as the home
/// is: on Windows `~/.claude-work` is `C:\Users\me\.claude-work`, not a
/// path with both separators, which Windows accepts but which then differs
/// as text from the same path elsewhere (in a report, or an environment
/// variable compared by value).
fn expand(value: &str, home: Option<&str>) -> Option<String> {
    let rest = if value == "~" {
        ""
    } else if let Some(rest) = value.strip_prefix("~/") {
        rest
    } else {
        return Some(value.to_owned());
    };
    let home = home?;
    let rest = rest.replace('/', std::path::MAIN_SEPARATOR_STR);
    let separator = if home.ends_with(['/', std::path::MAIN_SEPARATOR]) || rest.is_empty() {
        ""
    } else {
        std::path::MAIN_SEPARATOR_STR
    };
    Some(format!("{home}{separator}{rest}"))
}

/// The TOML library's message without the value it found. Serde's type
/// and value errors quote it (``invalid type: integer `98765`, expected a
/// string``, `invalid type: string "--token=…", expected a sequence`),
/// and a value in `env` or `args` may be a secret that `profile.reload`
/// would send to a client. Only the kind found and what was expected stay.
/// Syntax errors, unknown and missing fields name keys and positions only
/// (checked against `toml_edit` 0.25), so they pass as they are.
fn without_values(message: &str) -> String {
    for prefix in ["invalid type: ", "invalid value: "] {
        let Some(at) = message.find(prefix) else {
            continue;
        };
        let (before, rest) = message.split_at(at);
        let rest = rest.get(prefix.len()..).unwrap_or_default();
        // What was expected is written by code, never taken from the file,
        // so the last separator is the real one even if the value holds it.
        return match rest.rsplit_once(", expected ") {
            Some((found, expected)) => {
                let kind = found.split(['`', '"']).next().unwrap_or_default().trim();
                format!("{before}{prefix}{kind}, expected {expected}")
            }
            None => format!("{before}{prefix}a value of another type"),
        };
    }
    message.to_owned()
}

/// A place in `profiles.toml`; both count from 1, the column in
/// characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// The line.
    pub line: usize,
    /// The column.
    pub column: usize,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

/// The position of a byte offset.
fn position(text: &str, span: Option<std::ops::Range<usize>>) -> Option<Position> {
    let before = span.and_then(|span| text.get(..span.start))?;
    let line = before.matches('\n').count().saturating_add(1);
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |line| line.chars().count())
        .saturating_add(1);
    Some(Position { line, column })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::unnecessary_wraps,
        reason = "parse_profiles takes an optional home"
    )]
    fn home() -> Option<&'static Path> {
        Some(Path::new("/home/u"))
    }

    /// `name` under [`home`], as `~/name` expands: the system's separator.
    fn under_home(name: &str) -> String {
        format!("/home/u{}{name}", std::path::MAIN_SEPARATOR)
    }

    fn var(name: &str) -> EnvName {
        name.parse().unwrap()
    }

    /// Review finding 5: lookups follow the system's case rule, so a
    /// profile's `codex_home` is Codex's home on Windows, where the
    /// environment ignores case, and another variable elsewhere.
    #[test]
    fn lookups_follow_the_systems_case_rule() {
        let text = "[profiles.x]\nharness = \"codex\"\nenv = { codex_home = \"/w\" }\nunset = [\"Path\"]\n";
        let x = parse_profiles(text, home())
            .unwrap()
            .get(&"x".parse().unwrap())
            .cloned()
            .unwrap();
        let windows = cfg!(windows);
        assert_eq!(x.get(&var("CODEX_HOME")).is_some(), windows);
        assert_eq!(x.unsets(&var("PATH")), windows);
        assert_eq!(x.get(&var("codex_home")), Some("/w"));
        assert!(x.unsets(&var("Path")));
        assert_eq!(same_variable("Path", "PATH"), windows);
        assert!(same_variable("PATH", "PATH"));
    }

    fn problem(text: &str) -> ProfileProblem {
        match parse_profiles(text, home()) {
            Err(ProfilesError::Profile { problem, .. }) => problem,
            other => panic!("expected a profile problem, got {other:?}"),
        }
    }

    const ALT: &str = r#"
        [profiles.claude-alt]
        harness = "claude"
        label = "Claude (Alt)"
        env = { CLAUDE_CONFIG_DIR = "~/.claude-alt", ANTHROPIC_BASE_URL = "https://example.invalid" }
        unset = ["ANTHROPIC_API_KEY"]
        args = ["--model", "opus", "--settings", "~/s.json"]

        [profiles.codex-work]
        harness = "codex"
        env = { CODEX_HOME = "~" }
    "#;

    #[test]
    fn the_adr_example_parses() {
        let profiles = parse_profiles(ALT, home()).unwrap();
        let alt = profiles.get(&"claude-alt".parse().unwrap()).unwrap();
        assert_eq!(alt.harness(), HarnessKind::ClaudeCode);
        assert_eq!(alt.label(), Some("Claude (Alt)"));
        assert_eq!(
            alt.get(&var("CLAUDE_CONFIG_DIR")),
            Some(under_home(".claude-alt").as_str())
        );
        assert!(alt.unsets(&var("ANTHROPIC_API_KEY")));
        assert_eq!(
            alt.args(),
            [
                "--model",
                "opus",
                "--settings",
                under_home("s.json").as_str()
            ]
        );
        assert!(!alt.is_default());
        let work = profiles.get(&"codex-work".parse().unwrap()).unwrap();
        assert_eq!(work.get(&var("CODEX_HOME")), Some("/home/u"));
        let names: Vec<_> = profiles.all().map(|p| p.name().to_string()).collect();
        assert_eq!(
            names,
            [
                "claude",
                "codex",
                "gemini",
                "opencode",
                "claude-alt",
                "codex-work"
            ]
        );
    }

    #[test]
    fn defaults_change_nothing_and_are_found_by_name() {
        let profiles = parse_profiles("", home()).unwrap();
        for harness in HarnessKind::ALL {
            let default = Profile::default_for(harness);
            assert!(default.is_default());
            assert_eq!(default.env().count(), 0);
            assert_eq!(default.unset().count(), 0);
            assert!(default.args().is_empty());
            assert_eq!(profiles.get(default.name()), Some(&default));
        }
        assert_eq!(
            Profile::default_for(HarnessKind::ClaudeCode)
                .name()
                .as_str(),
            "claude"
        );
        assert_eq!(profiles.get(&"nope".parse().unwrap()), None);
    }

    #[test]
    fn values_reach_neither_info_nor_debug() {
        let text = r#"
            [profiles.keyed]
            harness = "claude"
            env = { ANTHROPIC_API_KEY = "sk-very-secret" }
        "#;
        let profiles = parse_profiles(text, home()).unwrap();
        let keyed = profiles.get(&"keyed".parse().unwrap()).unwrap();
        let info = keyed.info();
        assert_eq!(info.sets, ["ANTHROPIC_API_KEY".parse::<EnvName>().unwrap()]);
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("sk-very-secret"), "{json}");
        for shown in [format!("{keyed:?}"), format!("{profiles:?}")] {
            assert!(!shown.contains("sk-very-secret"), "{shown}");
            assert!(shown.contains("ANTHROPIC_API_KEY"), "{shown}");
        }
    }

    #[test]
    fn toml_errors_name_the_key_and_position_without_quoting_the_file() {
        let text = "[profiles.x]\nharness = \"claude\"\nenv = { KEY = \"sk-secret\" \n";
        let error = parse_profiles(text, home()).unwrap_err();
        let shown = error.to_string();
        assert!(!shown.contains("sk-secret"), "{shown}");
        assert!(shown.contains("line 3"), "{shown}");

        let unknown = "[profiles.x]\nharness = \"claude\"\nenviron = {}\n";
        let shown = parse_profiles(unknown, home()).unwrap_err().to_string();
        assert!(shown.contains("environ"), "{shown}");
        assert!(shown.contains("line 3"), "{shown}");

        let missing = "[profiles.x]\nlabel = \"no harness\"\n";
        let shown = parse_profiles(missing, home()).unwrap_err().to_string();
        assert!(shown.contains("harness"), "{shown}");

        let top = "[profile.x]\nharness = \"claude\"\n";
        let shown = parse_profiles(top, home()).unwrap_err().to_string();
        assert!(shown.contains("profile"), "{shown}");

        let typed = "[profiles.x]\nharness = \"claude\"\nunset = \"HOME\"\n";
        let shown = parse_profiles(typed, home()).unwrap_err().to_string();
        assert!(shown.contains("line 3"), "{shown}");
    }

    #[test]
    fn toml_errors_carry_a_typed_position() {
        let error =
            parse_profiles("[profiles.x]\nharness = \"claude\"\nenv = 3\n", home()).unwrap_err();
        let ProfilesError::Toml { position, .. } = error else {
            panic!("expected a TOML error");
        };
        assert_eq!(position, Some(Position { line: 3, column: 7 }));
    }

    /// Review finding: serde's messages quoted the value they found, and a
    /// value may be a secret.
    #[test]
    fn errors_never_quote_a_value() {
        for (text, kind) in [
            ("env = { PIN = 98765432123 }", "integer, expected a string"),
            (
                "args = \"--token=sk-SECRET\"",
                "string, expected a sequence",
            ),
            ("env = \"sk-SECRET\"", "string, expected a map"),
            ("unset = \"sk-SECRET\"", "string, expected a sequence"),
            ("label = 1.5e300", "floating point, expected a string"),
            ("env = { K = true }", "boolean, expected a string"),
            (
                "env = { K = 98765432123.5 }",
                "floating point, expected a string",
            ),
        ] {
            let text = format!("[profiles.x]\nharness = \"claude\"\n{text}\n");
            let shown = parse_profiles(&text, home()).unwrap_err().to_string();
            for value in ["98765432123", "sk-SECRET", "1500000", "true"] {
                assert!(!shown.contains(value), "{value} in {shown}");
            }
            assert!(shown.contains(kind), "{shown}");
            assert!(shown.contains("line 3"), "{shown}");
        }
        for text in [
            "profiles = \"sk-SECRET\"\n",
            "[profiles]\nx = \"sk-SECRET\"\n",
            "[profiles.x]\nharness = \"claude\"\nenv = { K = sk-SECRET }\n",
            "[profiles.x]\nharness = \"claude\"\nenv = { K = \"\\q sk-SECRET\" }\n",
            "[profiles.x]\nharness = \"claude\"\nenv = { K = \"a\" } sk-SECRET\n",
        ] {
            let shown = parse_profiles(text, home()).unwrap_err().to_string();
            assert!(!shown.contains("SECRET"), "{shown}");
        }
        assert_eq!(
            without_values("invalid value: string \"a, expected b\", expected one of x"),
            "invalid value: string, expected one of x"
        );
        assert_eq!(
            without_values("invalid type: integer `5`"),
            "invalid type: a value of another type"
        );
        assert_eq!(
            without_values("missing field `harness`"),
            "missing field `harness`"
        );
    }

    #[test]
    fn names_are_checked() {
        assert!(matches!(
            problem("[profiles.\"bad name\"]\nharness = \"claude\"\n"),
            ProfileProblem::InvalidName(_)
        ));
        for reserved in ["claude", "Claude-Code", "codex", "gemini", "OpenCode"] {
            let text = format!("[profiles.{reserved}]\nharness = \"codex\"\n");
            assert!(
                matches!(problem(&text), ProfileProblem::Reserved { .. }),
                "{reserved}"
            );
        }
        let twice = "[profiles.Work]\nharness = \"codex\"\n[profiles.work]\nharness = \"claude\"\n";
        let error = parse_profiles(twice, home()).unwrap_err();
        assert!(
            matches!(
                &error,
                ProfilesError::Profile {
                    problem: ProfileProblem::Duplicate { .. },
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("work") || error.to_string().contains("Work"));
    }

    #[test]
    fn harness_and_variables_are_checked() {
        assert_eq!(
            problem("[profiles.x]\nharness = \"vim\"\n"),
            ProfileProblem::UnknownHarness {
                value: "vim".to_owned()
            }
        );
        assert_eq!(
            problem("[profiles.x]\nharness = \"claude\"\nenv = { \"BAD-NAME\" = \"1\" }\n"),
            ProfileProblem::InvalidVariable {
                key: "env",
                name: "BAD-NAME".to_owned()
            }
        );
        assert_eq!(
            problem("[profiles.x]\nharness = \"claude\"\nunset = [\"1X\"]\n"),
            ProfileProblem::InvalidVariable {
                key: "unset",
                name: "1X".to_owned()
            }
        );
        assert!(matches!(
            problem("[profiles.x]\nharness = \"claude\"\nenv = { Path = \"a\", PATH = \"b\" }\n"),
            ProfileProblem::RepeatedVariable { key: "env", .. }
        ));
        assert!(matches!(
            problem("[profiles.x]\nharness = \"claude\"\nunset = [\"A\", \"a\"]\n"),
            ProfileProblem::RepeatedVariable { key: "unset", .. }
        ));
        assert_eq!(
            problem(
                "[profiles.x]\nharness = \"claude\"\nenv = { HOME = \"/h\" }\nunset = [\"home\"]\n"
            ),
            ProfileProblem::SetAndUnset {
                name: "home".parse().unwrap()
            }
        );
        let error = parse_profiles(
            "[profiles.x]\nharness = \"claude\"\nunset = [\"A\", \"a\"]\n",
            home(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("profile `x`") && error.contains("`unset`"),
            "{error}"
        );
    }

    #[test]
    fn tilde_needs_a_home() {
        let text = "[profiles.x]\nharness = \"claude\"\nenv = { CLAUDE_CONFIG_DIR = \"~/.c\" }\n";
        let error = parse_profiles(text, None).unwrap_err();
        assert!(matches!(
            error,
            ProfilesError::Profile { problem: ProfileProblem::NoHome { ref key }, .. } if key == "env.CLAUDE_CONFIG_DIR"
        ));
        let plain =
            "[profiles.x]\nharness = \"claude\"\nenv = { A = \"~user/x\" }\nargs = [\"a~/b\"]\n";
        let profiles = parse_profiles(plain, None).unwrap();
        let x = profiles.get(&"x".parse().unwrap()).unwrap();
        assert_eq!(x.get(&var("A")), Some("~user/x"));
        assert_eq!(x.args(), ["a~/b"]);
        assert_eq!(expand("~/a", Some("/h/")), Some("/h/a".to_owned()));
        assert_eq!(expand("~", Some("/h")), Some("/h".to_owned()));
        // The system's separator throughout, as `Path::join` writes it.
        let home = std::env::temp_dir();
        let home_text = home.to_str().unwrap().trim_end_matches(['/', '\\']);
        assert_eq!(
            expand("~/.claude-work/sub", Some(home_text)),
            Some(
                Path::new(home_text)
                    .join(".claude-work")
                    .join("sub")
                    .display()
                    .to_string()
            )
        );
    }
}
