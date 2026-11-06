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
use std::path::{Path, PathBuf};

use serde::Serialize;
use vessel_proto::{HarnessSessionId, McpTransport, ResolvedMcpServers};

use super::{Descriptor, Home, Structured};
use crate::layout::HarnessLayout;
use crate::mcp::{FileForm, McpError, TerminalMcp};
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
    sessions: Some(session_files),
    terminal_mcp: Some(terminal_mcp),
    terminal_args: true,
};

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

/// One server in `--mcp-config`'s file, `{"mcpServers": {"<name>": ...}}`.
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

/// The servers, values included, in a file only the user can read.
fn terminal_mcp(servers: &ResolvedMcpServers) -> Result<TerminalMcp, McpError> {
    let mut encoded = serde_json::Map::new();
    for (name, transport) in servers {
        let encode = |reason: String| McpError::Encode {
            server: name.clone(),
            reason,
        };
        let server = match transport {
            McpTransport::Stdio { command, args, env } => Server::Stdio {
                command: command
                    .to_str()
                    .ok_or_else(|| encode("its command is not valid UTF-8".to_owned()))?,
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
        let value = serde_json::to_value(server).map_err(|error| encode(error.to_string()))?;
        let _old = encoded.insert(name.as_str().to_owned(), value);
    }
    let mut config = serde_json::Map::new();
    let _old = config.insert("mcpServers".to_owned(), serde_json::Value::Object(encoded));
    Ok(TerminalMcp {
        file: Some(FileForm {
            flag: "--mcp-config=",
            contents: serde_json::Value::Object(config).to_string(),
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
