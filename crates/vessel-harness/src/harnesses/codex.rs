//! The `OpenAI` Codex CLI (`codex`).
//!
//! Structured mode goes through Zed's ACP adapter, `codex-acp`, which embeds
//! Codex and reads the same `~/.codex` configuration. The Codex CLI itself
//! speaks its own app-server protocol, not ACP (ADR 0003 explains why Vessel
//! does not implement per-harness protocols).
//!
//! Checked 2026-11-06 against Codex CLI 0.156.1 (`--help` with a throwaway
//! home, and the file names in a real home, never contents):
//!
//! - Home: `CODEX_HOME`, default `~/.codex`.
//! - `codex login status` reports the login and changes nothing.
//! - `-c key=value` overrides one configuration key for this run, the value
//!   parsed as TOML; nothing is written to the home. MCP servers are the
//!   `mcp_servers.<name>` tables: `command`, `args`, `env_vars` (variables
//!   forwarded from Codex's own environment), `url` and `env_http_headers`
//!   (headers read from Codex's environment). Literal `env` and
//!   `http_headers` exist too; Vessel never uses them, since they would put
//!   values on the command line.
//! - A session is `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use toml_edit::{Array, InlineTable, Value};
use vessel_proto::{
    EnvName, HarnessKind, HarnessSessionId, McpServerName, McpTransport, ResolvedMcpServers,
    ResolvedValue,
};

use super::{Descriptor, Home, Structured};
use crate::layout::HarnessLayout;
use crate::mcp::{EncodeProblem, McpError, TerminalMcp};
use crate::sessions::{SessionFiles, SessionFilesError, entry};

pub(super) const DESCRIPTOR: Descriptor = Descriptor {
    binary: "codex",
    profile: "codex",
    structured: Structured::Adapter {
        binary: "codex-acp",
        install: "npm install -g @zed-industries/codex-acp",
    },
    home: Home {
        variable: Some("CODEX_HOME"),
        default: &[".codex"],
    },
    layout: HarnessLayout {
        instructions: Some("AGENTS.md"),
        skills: Some("skills"),
    },
    login: Some(&["login", "status"]),
    install: "npm install -g @openai/codex",
    login_command: Some(&["login"]),
    account_files: &["auth.json"],
    sessions: Some(session_files),
    terminal_mcp: Some(terminal_mcp),
    terminal_args: true,
};

/// Finds `sessions/*/*/*/rollout-<timestamp>-<id>.jsonl`, the id matched
/// whole (a suffix of a longer id is another session). Symbolic links are
/// not followed. Two files for one id is an error rather than a guess.
fn session_files(
    home: &Path,
    _cwd: &Path,
    id: &HarnessSessionId,
) -> Result<Option<SessionFiles>, SessionFilesError> {
    let mut level = vec![PathBuf::from("sessions")];
    for _depth in 0..3 {
        let mut next = Vec::new();
        for dir in &level {
            next.extend(children(home, dir, Metadata::is_dir)?);
        }
        level = next;
    }
    let mut found = Vec::new();
    for day in &level {
        for file in children(home, day, Metadata::is_file)? {
            let matches = file
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| rollout_id(name) == Some(id.as_str()));
            if matches {
                found.push(file);
            }
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop().map(|file| SessionFiles::new(file, Vec::new()))),
        count => Err(SessionFilesError::Ambiguous {
            harness: HarnessKind::Codex,
            id: id.clone(),
            count,
        }),
    }
}

/// The session id in a rollout file name,
/// `rollout-YYYY-MM-DDTHH-MM-SS-<id>.jsonl`; `None` for any other name.
fn rollout_id(name: &str) -> Option<&str> {
    const TIMESTAMP: usize = "YYYY-MM-DDTHH-MM-SS".len();
    let rest = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let (timestamp, id) = (rest.get(..TIMESTAMP)?, rest.get(TIMESTAMP..)?);
    let shaped = timestamp.char_indices().all(|(at, character)| match at {
        4 | 7 | 13 | 16 => character == '-',
        10 => character == 'T',
        _ => character.is_ascii_digit(),
    });
    let id = id.strip_prefix('-')?;
    (shaped && !id.is_empty()).then_some(id)
}

/// Entries of `home/relative` whose own metadata (not a link's target)
/// passes `keep`, as paths relative to `home`, sorted; none if it is not a
/// directory.
fn children(
    home: &Path,
    relative: &Path,
    keep: impl Fn(&Metadata) -> bool,
) -> Result<Vec<PathBuf>, SessionFilesError> {
    let dir = home.join(relative);
    match entry(&dir)? {
        Some(metadata) if metadata.is_dir() => {}
        Some(_) | None => return Ok(Vec::new()),
    }
    let io = |source| SessionFilesError::Io {
        path: dir.clone(),
        source,
    };
    let mut found = Vec::new();
    for child in std::fs::read_dir(&dir).map_err(io)? {
        let child = child.map_err(io)?;
        let path = relative.join(child.file_name());
        if entry(&home.join(&path))?.is_some_and(|metadata| keep(&metadata)) {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// One `-c mcp_servers.<name>={...}` per server, replacing any server of
/// that name in the user's configuration whole (a partial override would
/// mix the two). Values go into Codex's environment: a stdio server's under
/// its own names, forwarded with `env_vars`; a header's under a generated
/// name, read with `env_http_headers`.
fn terminal_mcp(servers: &ResolvedMcpServers) -> Result<TerminalMcp, McpError> {
    let mut mcp = TerminalMcp::default();
    let mut env = Environment::default();
    for (index, (name, transport)) in servers.iter().enumerate() {
        let mut table = InlineTable::new();
        match transport {
            McpTransport::Stdio {
                command,
                args,
                env: variables,
            } => {
                let command = command.to_str().ok_or_else(|| McpError::Encode {
                    server: name.clone(),
                    problem: EncodeProblem::CommandNotUtf8,
                })?;
                let _old = table.insert("command", Value::from(command));
                if !args.is_empty() {
                    let _old = table.insert("args", Value::Array(args.iter().collect::<Array>()));
                }
                if !variables.is_empty() {
                    let names = variables.keys().map(EnvName::as_str).collect::<Array>();
                    let _old = table.insert("env_vars", Value::Array(names));
                    for (variable, value) in variables {
                        env.add(variable.clone(), value)?;
                    }
                }
            }
            McpTransport::Http { url, headers } => {
                let _old = table.insert("url", Value::from(url.as_str()));
                if !headers.is_empty() {
                    let mut forwarded = InlineTable::new();
                    for (header_index, (header, value)) in headers.iter().enumerate() {
                        let variable = header_variable(name, index, header_index)?;
                        let _old =
                            forwarded.insert(header.as_str(), Value::from(variable.as_str()));
                        env.add(variable, value)?;
                    }
                    let _old = table.insert("env_http_headers", Value::InlineTable(forwarded));
                }
            }
        }
        mcp.args.push("-c".into());
        mcp.args
            .push(format!("mcp_servers.{name}={}", Value::InlineTable(table)).into());
    }
    mcp.env = env.values.into_values().collect();
    Ok(mcp)
}

/// `VESSEL_MCP_<server>_<header>_SECRET`: indexes, since server and header
/// names may hold characters a variable name cannot; the `_SECRET` ending
/// makes name-based filters (Codex's own default for the commands it runs
/// among them) leave the value out.
fn header_variable(
    server: &McpServerName,
    index: usize,
    header_index: usize,
) -> Result<EnvName, McpError> {
    format!("VESSEL_MCP_{index}_{header_index}_SECRET")
        .parse()
        .map_err(|_invalid| McpError::Encode {
            server: server.clone(),
            problem: EncodeProblem::HeaderVariable,
        })
}

/// Variables for Codex's environment, one value per name (ignoring case,
/// as Windows does).
#[derive(Default)]
struct Environment {
    values: BTreeMap<String, (EnvName, ResolvedValue)>,
}

impl Environment {
    fn add(&mut self, name: EnvName, value: &ResolvedValue) -> Result<(), McpError> {
        let folded = name.as_str().to_ascii_uppercase();
        match self.values.get(&folded) {
            Some((_, existing)) if existing == value => Ok(()),
            Some(_) => Err(McpError::SharedVariable { name }),
            None => {
                let _new = self.values.insert(folded, (name, value.clone()));
                Ok(())
            }
        }
    }
}
