//! MCP servers for terminal sessions, given at launch (ADR 0012).
//!
//! Structured sessions get their MCP servers over ACP (`session/new`,
//! `session/load`, in `vessel-agent`). A terminal session runs the
//! harness's own TUI, so each harness's launch-time form is used instead:
//!
//! - Claude Code: `--mcp-config=<file>`, a JSON file (`{"mcpServers": …}`)
//!   written owner-only for the session and removed with it ([`McpFile`]).
//! - Codex: `-c mcp_servers.<name>.…` overrides. Codex has no file flag
//!   (its `--profile` layers a file inside its own home), so values reach
//!   the server by name: `env_vars` forwards variables from Codex's own
//!   environment, `env_http_headers` reads headers from it. Those values
//!   are then in Codex's own environment, which the commands the agent runs
//!   may inherit (Codex filters names holding `KEY`, `SECRET` or `TOKEN` by
//!   default, so generated names end in `_SECRET`); Claude Code's values
//!   stay in its file. A variable that would change Codex itself is
//!   refused: one the profile or the inherited environment already gives
//!   another value, and those steering its search path, home, loader,
//!   proxies or credentials (`PATH`, `HOME`, `LD_*`, `DYLD_*`,
//!   `NODE_OPTIONS`, `*_PROXY`, `OPENAI_*`, `CODEX_*`, and the Windows
//!   equivalents).
//! - Gemini CLI, `OpenCode`: no launch-time form that leaves the user's own
//!   configuration alone was confirmed (2026-11-06); their terminal
//!   sessions start without the servers and the caller reports which
//!   ([`Launch::unattached`](crate::Launch::unattached)).
//!
//! [Decision Log]
//! - Constraint: no value of an MCP server ever goes on a command line.
//!   Arguments are visible to every local user (`ps`); values may be
//!   secrets. The types already keep values out of `args`
//!   (`vessel_proto::McpTransport`); this module keeps them out of the
//!   harness's arguments too, putting them in an owner-only file or in the
//!   process environment (readable only by the same user).
//! - The file lives in a directory the caller must give
//!   ([`launch`](crate::launch); the node's is owner-only under its data
//!   directory and emptied at start, so a crash leaves nothing behind for
//!   long, ADR 0012), under a random name, created exclusively (an existing
//!   file or link there is refused, never followed) and owner-only. There
//!   is no fallback to a shared temporary directory. The caller writes it
//!   right before starting the harness and keeps the guard for the
//!   session's life; dropping the guard removes it.
//! - Windows has no mode bits, and setting an explicit DACL needs Win32
//!   calls the workspace does not make (no `unsafe`). A file there inherits
//!   its directory's ACL, which inside the user's profile admits only that
//!   user, the guarantee the node's `token` and `node.key` already rely on
//!   (`vessel-store`'s `files`). So on Windows the file is written only
//!   inside the user's profile directory and refused anywhere else.

use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use vessel_proto::{EnvName, McpServerName, ResolvedValue};

/// MCP servers could not be given to a terminal session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    /// Two values for one environment variable: Codex forwards variables
    /// by name from its own environment, so two servers, a server and the
    /// profile, or a server and the environment Codex inherits cannot give
    /// the same variable different values.
    #[error("the variable {name} would need two different values for this harness's MCP servers")]
    SharedVariable {
        /// The variable.
        name: EnvName,
    },
    /// A variable that steers the harness itself (its search path, home,
    /// loader, proxy or credentials), which an MCP server may not set in
    /// the harness's own environment.
    #[error(
        "an MCP server may not set {name} for this harness: it would change the harness itself"
    )]
    ProtectedVariable {
        /// The variable.
        name: EnvName,
    },
    /// A server's definition could not be encoded for the harness.
    #[error("MCP server {server} cannot be written for this harness: {reason}")]
    Encode {
        /// The server.
        server: McpServerName,
        /// Why.
        reason: String,
    },
}

/// A harness's launch-time MCP form, before a file path is chosen.
#[derive(Debug, Default)]
pub(crate) struct TerminalMcp {
    /// Arguments; never a value.
    pub(crate) args: Vec<OsString>,
    /// Variables to set in the harness's environment (values).
    pub(crate) env: Vec<(EnvName, ResolvedValue)>,
    /// A file to write, and the flag that names it on the command line.
    pub(crate) file: Option<FileForm>,
}

/// A file the harness reads its MCP servers from.
pub(crate) struct FileForm {
    /// The argument the file's path is appended to, as one argument
    /// (`--mcp-config=`).
    pub(crate) flag: &'static str,
    /// The file's contents (holds values).
    pub(crate) contents: String,
    /// The file name's ending, `.json` for example.
    pub(crate) extension: &'static str,
}

impl fmt::Debug for FileForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileForm")
            .field("flag", &self.flag)
            .finish_non_exhaustive()
    }
}

/// A per-session MCP configuration file for a terminal session. Its
/// contents may hold secrets: `Debug` shows the path only. Write it with
/// [`McpFile::write`] right before starting the session.
pub struct McpFile {
    path: PathBuf,
    contents: String,
}

impl fmt::Debug for McpFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpFile")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl McpFile {
    /// A file at a new random path in `dir`.
    pub(crate) fn new(form: FileForm, dir: &Path) -> io::Result<Self> {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let mut name = String::from("vessel-mcp-");
        for byte in random {
            let _infallible = write!(name, "{byte:02x}");
        }
        name.push_str(form.extension);
        Ok(Self {
            path: dir.join(name),
            contents: form.contents,
        })
    }

    /// Where the harness is told to read it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create the file, exclusively and owner-only; it is removed when the
    /// returned guard is dropped.
    ///
    /// # Errors
    /// The file exists already (never overwritten or followed) or cannot be
    /// written.
    pub fn write(self) -> io::Result<WrittenMcpFile> {
        let mut file = new_private(&self.path)?;
        let written = file
            .write_all(self.contents.as_bytes())
            .and_then(|()| file.sync_all());
        let guard = WrittenMcpFile { path: self.path };
        written.map(|()| guard)
    }
}

/// A written [`McpFile`]; removes it when dropped (the session ended).
#[derive(Debug)]
pub struct WrittenMcpFile {
    path: PathBuf,
}

impl WrittenMcpFile {
    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WrittenMcpFile {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), %error, "could not remove an MCP configuration file");
        }
    }
}

#[cfg(unix)]
fn new_private(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Windows: only inside the user's profile directory, whose inherited ACL
/// admits only that user (see the module docs); refused elsewhere.
#[cfg(not(unix))]
fn new_private(path: &Path) -> io::Result<fs::File> {
    let inside_profile = path
        .parent()
        .and_then(|dir| fs::canonicalize(dir).ok())
        .zip(dirs::home_dir().and_then(|home| fs::canonicalize(home).ok()))
        .is_some_and(|(dir, home)| dir.starts_with(home));
    if !inside_profile {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "an MCP configuration file is written only inside the user's profile directory on Windows",
        ));
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}
