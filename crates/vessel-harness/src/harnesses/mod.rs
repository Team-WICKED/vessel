//! Per-harness knowledge, one module per [`HarnessKind`].
//!
//! [`descriptor`] is the only `match` on `HarnessKind` that decides
//! behaviour in the whole workspace. Adding a harness variant fails to
//! compile here until its module exists. Where a harness needs logic rather
//! than data (finding a transcript, writing MCP servers), its module has a
//! function and the descriptor points to it.

mod claude;
mod codex;
mod gemini;
mod opencode;

use std::path::Path;

use vessel_proto::{HarnessKind, ResolvedMcpServers};

use crate::layout::HarnessLayout;
use crate::mcp::{McpError, TerminalMcp};
use crate::sessions::{SessionFiles, SessionFilesError};

/// What Vessel needs to know to run one harness.
#[derive(Debug)]
pub(crate) struct Descriptor {
    /// Executable name looked up on the search path (without `.exe`).
    pub(crate) binary: &'static str,
    /// The name of the harness's implicit default profile (ADR 0011): its
    /// command, which is what people type.
    pub(crate) profile: &'static str,
    /// How structured (ACP) mode is started.
    pub(crate) structured: Structured,
    /// Where the harness keeps its configuration, logins and sessions.
    pub(crate) home: Home,
    /// Where shared configuration (instructions, skills) lives in the home.
    pub(crate) layout: HarnessLayout,
    /// Arguments after the binary that report whether it is logged in,
    /// without changing anything; `None` if the harness has no such command.
    pub(crate) login: Option<&'static [&'static str]>,
    /// How to find one session's files under the home; `None` if they
    /// cannot be moved between homes.
    pub(crate) sessions: Option<FindSession>,
    /// How a terminal session is given MCP servers at launch; `None` if the
    /// harness has no launch-time form (structured sessions still get them
    /// over ACP).
    pub(crate) terminal_mcp: Option<RenderMcp>,
    /// Whether a profile may add arguments to its terminal command line.
    /// Every harness so far takes them; a harness whose terminal command
    /// must stay fixed would say `false`, and its profiles with `args` are
    /// refused when `profiles.toml` is read.
    pub(crate) terminal_args: bool,
}

/// Find a session's files: `(home, working directory, session id)`, the
/// id already checked to be one plain file name.
pub(crate) type FindSession =
    fn(&Path, &Path, &str) -> Result<Option<SessionFiles>, SessionFilesError>;

/// Render MCP servers into the harness's launch-time form.
pub(crate) type RenderMcp = fn(&ResolvedMcpServers) -> Result<TerminalMcp, McpError>;

/// How structured mode starts.
#[derive(Debug)]
pub(crate) enum Structured {
    /// The harness binary itself speaks ACP when given these arguments.
    Native {
        /// Arguments that switch the harness into ACP mode.
        args: &'static [&'static str],
    },
    /// A separate adapter program speaks ACP and drives the harness.
    Adapter {
        /// The adapter's executable name.
        binary: &'static str,
        /// How a user installs it; shown when it is missing.
        install: &'static str,
    },
}

/// A harness's home directory.
#[derive(Debug)]
pub(crate) struct Home {
    /// The environment variable that moves it, if the harness documents one.
    pub(crate) variable: Option<&'static str>,
    /// Where it is by default, relative to the user's home directory.
    pub(crate) default: &'static [&'static str],
}

/// The descriptor of `kind`.
pub(crate) const fn descriptor(kind: HarnessKind) -> &'static Descriptor {
    match kind {
        HarnessKind::ClaudeCode => &claude::DESCRIPTOR,
        HarnessKind::Codex => &codex::DESCRIPTOR,
        HarnessKind::Gemini => &gemini::DESCRIPTOR,
        HarnessKind::OpenCode => &opencode::DESCRIPTOR,
    }
}
