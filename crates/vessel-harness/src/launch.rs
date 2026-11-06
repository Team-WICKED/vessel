//! [`launch`]: turn a session spec, a profile and MCP servers into the
//! command that runs the session.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use vessel_proc::CommandSpec;
use vessel_proto::{McpServerName, ResolvedMcpServers, SessionMode, SessionSpec};

use crate::error::HarnessError;
use crate::harnesses::{Structured, descriptor};
use crate::lookup::Lookup;
use crate::mcp::{McpError, McpFile, TerminalMcp};
use crate::profile::Profile;

/// What [`launch`] decided: the command, and what it needs around it.
#[non_exhaustive]
pub struct Launch {
    /// The command. Its environment may hold secrets (the profile's values,
    /// MCP values for Codex); its arguments never hold an MCP value.
    pub command: CommandSpec,
    /// A terminal session's MCP configuration file, which the command
    /// names: write it ([`McpFile::write`]) right before starting the
    /// command and keep the guard until the session ends.
    pub mcp_file: Option<McpFile>,
    /// MCP servers this terminal session will not have, because its harness
    /// has no launch-time form for them. Structured sessions get every
    /// server over ACP, so this is empty for them.
    pub unattached: Vec<McpServerName>,
}

/// The program and arguments only: the environment may hold secrets.
impl fmt::Debug for Launch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variables: Vec<_> = self.command.env.iter().map(|(name, _)| name).collect();
        f.debug_struct("Launch")
            .field("program", &self.command.program)
            .field("args", &self.command.args)
            .field("env_names", &variables)
            .field("cwd", &self.command.cwd)
            .field("mcp_file", &self.mcp_file)
            .field("unattached", &self.unattached)
            .finish()
    }
}

/// The command that runs `spec` under `profile`, with `mcp` servers.
///
/// Terminal mode runs the harness binary with, in order: the MCP servers'
/// launch-time arguments, the profile's arguments, then the spec's.
/// Structured mode runs whatever speaks ACP for that harness: the binary
/// itself with its ACP arguments, or a separate adapter (which needs only
/// the adapter to be installed); its MCP servers go over ACP instead, so
/// `mcp` is not used, and neither are the profile's arguments. In both
/// modes the profile's variables are set and its `unset` ones removed.
///
/// The working directory defaults to the node user's home directory, is
/// made absolute, and must exist: running an agent in a different directory
/// than asked is worse than refusing.
///
/// Does filesystem lookups; call it from a blocking context.
///
/// # Errors
/// [`HarnessError::ProfileMismatch`] for another harness's profile;
/// [`HarnessError::NotInstalled`] or [`HarnessError::StructuredUnavailable`]
/// when the program is missing; [`HarnessError::RelativeDirectory`] or
/// [`HarnessError::NoSuchDirectory`] for a bad working directory;
/// [`HarnessError::Mcp`] when the servers cannot be given to this harness
/// (two values for one variable).
pub fn launch(
    lookup: &Lookup,
    spec: &SessionSpec,
    profile: &Profile,
    mcp: &ResolvedMcpServers,
) -> Result<Launch, HarnessError> {
    launch_in(lookup, spec, profile, mcp, &std::env::temp_dir())
}

/// [`launch`], with a terminal session's MCP configuration file in
/// `mcp_dir` rather than the system's temporary directory. The node passes
/// an owner-only directory of its own (ADR 0012); the file is owner-only
/// and created exclusively either way.
///
/// # Errors
/// As [`launch`].
pub fn launch_in(
    lookup: &Lookup,
    spec: &SessionSpec,
    profile: &Profile,
    mcp: &ResolvedMcpServers,
    mcp_dir: &Path,
) -> Result<Launch, HarnessError> {
    let harness = spec.harness;
    if profile.harness() != harness {
        return Err(HarnessError::ProfileMismatch {
            profile: profile.name().clone(),
            profile_harness: profile.harness(),
            harness,
        });
    }
    let descriptor = descriptor(harness);
    let installed = |binary: &'static str| {
        lookup
            .find(binary)
            .ok_or(HarnessError::NotInstalled { harness, binary })
    };
    let mut mcp_file = None;
    let mut unattached = Vec::new();
    let command = match &spec.mode {
        SessionMode::Terminal { args, .. } => {
            let mut command = CommandSpec::new(installed(descriptor.binary)?);
            match descriptor.terminal_mcp {
                Some(_) if mcp.is_empty() => {}
                Some(render) => {
                    let rendered = render(mcp)?;
                    check_environment(&rendered, profile, descriptor.home.variable)?;
                    let (with_servers, file) = attach(command, rendered, mcp_dir)?;
                    command = with_servers;
                    mcp_file = file;
                }
                None => unattached.extend(mcp.keys().cloned()),
            }
            command.args(profile.args()).args(args)
        }
        SessionMode::Structured { .. } => match &descriptor.structured {
            Structured::Native { args } => {
                CommandSpec::new(installed(descriptor.binary)?).args(args.iter())
            }
            Structured::Adapter { binary, install } => {
                let program =
                    lookup
                        .find(binary)
                        .ok_or_else(|| HarnessError::StructuredUnavailable {
                            harness,
                            reason: adapter_missing(binary, install),
                        })?;
                CommandSpec::new(program)
            }
        },
    };
    let command = with_profile(command, profile);
    let command = match working_dir(spec)? {
        Some(cwd) => command.cwd(cwd),
        None => command,
    };
    Ok(Launch {
        command,
        mcp_file,
        unattached,
    })
}

/// `command` with `profile`'s environment changes: removals first, then
/// the variables it sets (a profile never does both to one variable).
pub(crate) fn with_profile(command: CommandSpec, profile: &Profile) -> CommandSpec {
    let command = profile
        .unset()
        .fold(command, |command, name| command.env_remove(name.as_str()));
    profile.env().fold(command, |command, (name, value)| {
        command.env(name.as_str(), value)
    })
}

/// An MCP variable must not change what the profile decides, nor move the
/// harness's home: in Codex's environment it applies to Codex itself too.
fn check_environment(
    rendered: &TerminalMcp,
    profile: &Profile,
    home_variable: Option<&str>,
) -> Result<(), McpError> {
    for (name, value) in &rendered.env {
        let same = |other: &str| other.eq_ignore_ascii_case(name.as_str());
        let set_differently = profile
            .env()
            .any(|(key, set)| same(key.as_str()) && set != value.expose());
        let removed = profile.unset().any(|key| same(key.as_str()));
        if set_differently || removed || home_variable.is_some_and(same) {
            return Err(McpError::SharedVariable { name: name.clone() });
        }
    }
    Ok(())
}

/// The rendered form on the command: its arguments, its variables, and the
/// file's path as one argument.
fn attach(
    command: CommandSpec,
    rendered: TerminalMcp,
    dir: &Path,
) -> Result<(CommandSpec, Option<McpFile>), HarnessError> {
    let mut command = command.args(rendered.args);
    for (name, value) in rendered.env {
        command = command.env(name.as_str(), value.expose());
    }
    let Some(form) = rendered.file else {
        return Ok((command, None));
    };
    let mut argument = OsString::from(form.flag);
    let file = McpFile::new(form, dir).map_err(|error| HarnessError::McpFile {
        reason: error.to_string(),
    })?;
    argument.push(file.path());
    Ok((command.arg(argument), Some(file)))
}

/// Why structured mode is unavailable when an adapter is missing.
pub(crate) fn adapter_missing(binary: &str, install: &str) -> String {
    format!("its ACP adapter `{binary}` is not installed (install it with `{install}`)")
}

/// The spec's directory, which must be absolute, or else the home
/// directory; `None` only when the node user has no home directory, in
/// which case the node's own is inherited.
///
/// A relative directory is refused rather than resolved: it would be
/// resolved against `vesseld`'s own working directory, which the client
/// does not know and which depends on how the node was started, so the
/// session would silently run somewhere else than meant. Clients send
/// absolute paths (the CLI resolves its own relative ones). ACP needs an
/// absolute directory as well.
fn working_dir(spec: &SessionSpec) -> Result<Option<PathBuf>, HarnessError> {
    let cwd = match &spec.cwd {
        Some(cwd) if !cwd.is_absolute() => {
            return Err(HarnessError::RelativeDirectory { cwd: cwd.clone() });
        }
        Some(cwd) => cwd.clone(),
        // The node's own setting, so resolving it is the node's business.
        None => match dirs::home_dir().map(std::path::absolute) {
            Some(Ok(home)) => home,
            Some(Err(_)) | None => return Ok(None),
        },
    };
    if cwd.is_dir() {
        Ok(Some(cwd))
    } else {
        Err(HarnessError::NoSuchDirectory { cwd })
    }
}
