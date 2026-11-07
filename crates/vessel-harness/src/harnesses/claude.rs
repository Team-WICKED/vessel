//! Claude Code (`claude`).
//!
//! Structured mode goes through the ACP project's adapter,
//! `claude-agent-acp` (npm `@agentclientprotocol/claude-agent-acp`), built on
//! the Claude Agent SDK. The SDK runs Claude Code underneath and reads the
//! same home, so skills, MCP servers and permissions behave as in the
//! terminal. Claude Code itself does not speak ACP.
//!
//! Checked 2026-11-06 against Claude Code 2.1.318 (`--help` with a
//! throwaway home, and the file names in a real home, never contents):
//!
//! - Home: `CLAUDE_CONFIG_DIR`, default `~/.claude`. Each login is one home.
//! - `claude auth status` reports the login (JSON by default) and changes
//!   nothing.
//! - `--mcp-config <configs...>` loads MCP servers from JSON files, on top
//!   of the user's own (unless `--strict-mcp-config`, which Vessel does not
//!   pass). The option takes several values, so a path given as the next
//!   argument would also take the session's own arguments after it; Vessel
//!   passes `--mcp-config=<file>`, one argument.
//! - A session is `projects/<slug>/<id>.jsonl`, with `projects/<slug>/<id>/`
//!   (`subagents`, `tool-results`, `workflows`) and `file-history/<id>/`
//!   when they exist. `projects/<slug>/memory/` is the project's memory and
//!   stays per home.
//! - The slug (as Claude Code 2.1.318 computes it): the
//!   working directory with every UTF-16 unit other than an ASCII letter or
//!   digit replaced by `-` (`/home/u/.config/x` is `-home-u--config-x`; a
//!   character outside the BMP becomes two dashes). Past 200 characters it
//!   is the first 200, `-`, and the base-36 absolute value of the 32-bit
//!   string hash `h = h * 31 + unit` of the directory. The directory is the
//!   process's own (`getcwd`, symbolic links resolved), so a session is
//!   looked for under the given directory and under its canonical form.
//!   Some remote modes use a fixed project key instead; those sessions are
//!   not found.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use vessel_proto::{HarnessSessionId, McpTransport, ResolvedMcpServers};

use super::{Descriptor, Home, Structured};
use crate::layout::HarnessLayout;
use crate::mcp::{EncodeProblem, FileForm, McpError, TerminalMcp};
use crate::memory::{MemoryFilesError, RebaseError};
use crate::routing::{ModelProxy, Routed};
use crate::sessions::{SessionFiles, SessionFilesError, entry};

pub(super) const DESCRIPTOR: Descriptor = Descriptor {
    binary: "claude",
    profile: "claude",
    structured: Structured::Adapter {
        binary: "claude-agent-acp",
        install: "npm install -g @agentclientprotocol/claude-agent-acp",
    },
    home: Home {
        variable: Some("CLAUDE_CONFIG_DIR"),
        default: &[".claude"],
    },
    layout: HarnessLayout {
        instructions: Some("CLAUDE.md"),
        skills: Some("skills"),
    },
    login: Some(&["auth", "status"]),
    install: "npm install -g @anthropic-ai/claude-code",
    login_command: Some(&["auth", "login"]),
    account_files: &[".credentials.json", ".claude.json"],
    sessions: Some(session_files),
    memory: Some(memory_directories),
    rebase: Some(rebase),
    terminal_mcp: Some(terminal_mcp),
    terminal_args: true,
    model_proxy: Some(model_proxy),
};

/// A routed session (ADR 0015): Claude Code's documented gateway
/// variables. `ANTHROPIC_BASE_URL` without `/v1` (Claude Code adds the
/// path) and the session's token as `ANTHROPIC_AUTH_TOKEN`, which it sends
/// as `Authorization: Bearer` and prefers to every login but a cloud
/// provider's; so the cloud switches and an inherited `ANTHROPIC_API_KEY`
/// are removed (with both keys set it warns, and the route must not lose
/// to an account's key). Every model setting is pinned to the route's
/// models, so it asks for nothing the proxy would refuse: the main model,
/// the aliases (`opus`, `sonnet` to the model, `haiku` to the small model),
/// the background model and subagents.
fn model_proxy(proxy: &ModelProxy) -> Routed {
    let model = proxy.model.as_str().to_owned();
    let small = proxy.small().as_str().to_owned();
    Routed {
        env: vec![
            ("ANTHROPIC_BASE_URL", proxy.base_url.clone()),
            ("ANTHROPIC_AUTH_TOKEN", proxy.token.clone()),
            ("ANTHROPIC_MODEL", model.clone()),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", model.clone()),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL", model.clone()),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", small.clone()),
            ("ANTHROPIC_SMALL_FAST_MODEL", small),
            ("CLAUDE_CODE_SUBAGENT_MODEL", model),
        ],
        unset: &[
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ],
        args: Vec::new(),
    }
}

/// The longest slug Claude Code keeps whole.
const SLUG_MAX: usize = 200;

/// The project name Claude Code derives from a working directory; `None`
/// for a relative or non-Unicode one.
pub(crate) fn slug(cwd: &Path) -> Option<String> {
    if !cwd.is_absolute() {
        return None;
    }
    let text = cwd.to_str()?;
    let mut slug = String::with_capacity(text.len());
    let mut hash = 0_i32;
    for unit in text.encode_utf16() {
        let ascii = char::from_u32(u32::from(unit)).filter(char::is_ascii_alphanumeric);
        slug.push(ascii.unwrap_or('-'));
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit));
    }
    match slug.get(..SLUG_MAX) {
        Some(head) if slug.len() > SLUG_MAX => {
            Some(format!("{head}-{}", base36(hash.unsigned_abs())))
        }
        Some(_) | None => Some(slug),
    }
}

/// `n` in base 36, lowercase, as JavaScript's `toString(36)`.
fn base36(mut n: u32) -> String {
    let mut digits = Vec::new();
    loop {
        let digit = n
            .checked_rem(36)
            .and_then(|digit| char::from_digit(digit, 36));
        digits.push(digit.unwrap_or('0'));
        n = n.checked_div(36).unwrap_or(0);
        if n == 0 {
            break;
        }
    }
    digits.iter().rev().collect()
}

/// The working directory as given, then its canonical form if that differs
/// (Claude Code names the project after the resolved one).
fn directories(cwd: &Path) -> Vec<PathBuf> {
    std::iter::once(cwd.to_owned())
        .chain(canonical(cwd).filter(|canonical| canonical != cwd))
        .collect()
}

#[cfg(unix)]
fn canonical(cwd: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(cwd).ok()
}

/// Windows' canonical form is a `\\?\` path, which is not what a program
/// sees as its working directory, so only the given one is tried.
#[cfg(not(unix))]
fn canonical(_cwd: &Path) -> Option<PathBuf> {
    None
}

fn session_files(
    home: &Path,
    cwd: &Path,
    id: &HarnessSessionId,
) -> Result<Option<SessionFiles>, SessionFilesError> {
    let id = id.as_str();
    let mut found = None;
    for directory in directories(cwd) {
        let slug = slug(&directory).ok_or_else(|| SessionFilesError::InvalidDirectory {
            cwd: cwd.to_owned(),
        })?;
        let project = Path::new("projects").join(&slug);
        let transcript = project.join(format!("{id}.jsonl"));
        if entry(&home.join(&transcript))?.is_some_and(|metadata| metadata.file_type().is_file()) {
            found = Some((project, transcript));
            break;
        }
    }
    let Some((project, transcript)) = found else {
        return Ok(None);
    };
    let mut extras = Vec::new();
    for candidate in [project.join(id), PathBuf::from("file-history").join(id)] {
        if let Some(metadata) = entry(&home.join(&candidate))?
            && metadata.file_type().is_dir()
        {
            extras.push(candidate);
        }
    }
    Ok(Some(SessionFiles::new(transcript, extras)))
}

/// `projects/<slug>/memory/` of the project in `project` (under both
/// spellings of the directory, as for sessions), or of every project.
fn memory_directories(
    home: &Path,
    project: Option<&Path>,
) -> Result<Vec<PathBuf>, MemoryFilesError> {
    if let Some(cwd) = project {
        return directories(cwd)
            .iter()
            .map(|directory| {
                slug(directory)
                    .map(|slug| Path::new("projects").join(slug).join("memory"))
                    .ok_or_else(|| MemoryFilesError::InvalidDirectory {
                        cwd: cwd.to_owned(),
                    })
            })
            .collect();
    }
    let projects = home.join("projects");
    let entries = match std::fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(MemoryFilesError::Io {
                path: projects,
                source,
            });
        }
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| MemoryFilesError::Io {
            path: projects.clone(),
            source,
        })?;
        // Real directories only: a linked project is not walked into.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            found.push(Path::new("projects").join(entry.file_name()).join("memory"));
        }
    }
    found.sort();
    Ok(found)
}

/// A path under `projects/<project>/` moved under the slug of `to` (its
/// resolved form, which Claude Code names projects after); any other path
/// (`file-history/...`) as it is.
///
/// The project's own name is replaced whatever it is rather than matched
/// against `from`'s slug: a session's files, or one project's memory, are
/// all under one project, named after one of `from`'s spellings, and the
/// node renaming them may be unable to tell which (on a target node,
/// `from` exists only on the source, so its resolved spelling is
/// unknown). `from` must still be able to name a project.
fn rebase(path: &Path, from: &Path, to: &Path) -> Result<PathBuf, RebaseError> {
    let invalid = |cwd: &Path| RebaseError {
        cwd: cwd.to_owned(),
    };
    let _named = slug(from).ok_or_else(|| invalid(from))?;
    let resolved = directories(to).pop().unwrap_or_else(|| to.to_owned());
    let target = slug(&resolved).ok_or_else(|| invalid(to))?;
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(first)), Some(Component::Normal(_project)))
            if first == "projects" && components.clone().next().is_some() =>
        {
            Ok(Path::new("projects")
                .join(target)
                .join(components.as_path()))
        }
        _ => Ok(path.to_owned()),
    }
}

/// `--mcp-config`'s file: `{"mcpServers": {"<name>": {...}}}`.
#[derive(Serialize)]
struct McpConfig<'a> {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<&'a str, Server<'a>>,
}

/// One server in that file.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Server<'a> {
    Stdio {
        command: &'a str,
        args: &'a [String],
        env: BTreeMap<&'a str, &'a str>,
    },
    Http {
        url: &'a str,
        headers: BTreeMap<&'a str, &'a str>,
    },
}

/// The servers, values included, in a file only the user can read. The
/// file's text is the only copy made of the values.
fn terminal_mcp(servers: &ResolvedMcpServers) -> Result<TerminalMcp, McpError> {
    let Some(first) = servers.keys().next() else {
        return Ok(TerminalMcp::default());
    };
    let mut config = McpConfig {
        servers: BTreeMap::new(),
    };
    for (name, transport) in servers {
        let server = match transport {
            McpTransport::Stdio { command, args, env } => Server::Stdio {
                command: command.to_str().ok_or_else(|| McpError::Encode {
                    server: name.clone(),
                    problem: EncodeProblem::CommandNotUtf8,
                })?,
                args,
                env: env
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.expose()))
                    .collect(),
            },
            McpTransport::Http { url, headers } => Server::Http {
                url: url.as_str(),
                headers: headers
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.expose()))
                    .collect(),
            },
        };
        let _old = config.servers.insert(name.as_str(), server);
    }
    // About the whole file, so it names the first server; it cannot happen
    // (text and maps keyed by text only).
    let contents = serde_json::to_string(&config).map_err(|_unserialisable| McpError::Encode {
        server: first.clone(),
        problem: EncodeProblem::Unserialisable,
    })?;
    Ok(TerminalMcp {
        file: Some(FileForm {
            flag: "--mcp-config=",
            contents,
            extension: ".json",
        }),
        ..TerminalMcp::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute path on this platform: `/` on Unix, `C:\` on Windows,
    /// followed by `rest` with `/` as the separator.
    fn absolute(rest: &str) -> String {
        if cfg!(windows) {
            format!("C:\\{}", rest.replace('/', "\\"))
        } else {
            format!("/{rest}")
        }
    }

    #[test]
    fn slugs_follow_claude_code() {
        let slug = |rest: &str| slug(Path::new(&absolute(rest)));
        // `/` and `C:\` both start the slug with dashes.
        let start = if cfg!(windows) { "C--" } else { "-" };
        assert_eq!(
            slug("home/u/.config/work/x"),
            Some(format!("{start}home-u--config-work-x"))
        );
        assert_eq!(slug("srv/My_App-2"), Some(format!("{start}srv-My-App-2")));
        assert_eq!(slug("tmp/é"), Some(format!("{start}tmp--")));
        assert_eq!(slug("tmp/😀"), Some(format!("{start}tmp---")));
        assert_eq!(super::slug(Path::new("relative")), None);
    }

    /// Values computed with Claude Code 2.1.318's own slug function, run
    /// under Node, for both platforms' paths.
    #[test]
    fn long_slugs_are_cut_and_hashed_as_claude_code_does() {
        let long = absolute(&"a".repeat(250));
        let slug = slug(Path::new(&long)).unwrap();
        if cfg!(windows) {
            assert_eq!(slug, format!("C--{}-hltyrp", "a".repeat(197)));
        } else {
            assert_eq!(slug, format!("-{}-feo44x", "a".repeat(199)));
        }
        #[cfg(unix)]
        {
            let mixed = format!("/home/u/{}", "é😀x".repeat(80));
            let slug = super::slug(Path::new(&mixed)).unwrap();
            assert!(slug.starts_with("-home-u----x---x"), "{slug}");
            assert!(slug.ends_with("---x-wk7xa3"), "{slug}");
            assert_eq!(slug.len(), 207);
        }
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(2_147_483_648), "zik0zk");
    }
}
