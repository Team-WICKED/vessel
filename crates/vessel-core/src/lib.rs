//! Shared types and wire protocol for Vessel nodes.
//!
//! Everything a node, a CLI, or an editor client needs to talk about: node
//! identity, which agent harnesses a node can run, and the sessions it is
//! running. This crate has no I/O; it is pure data.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Wire protocol version. Bumped on any breaking change to these types.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default TCP port a node listens on.
pub const DEFAULT_PORT: u16 = 7077;

/// Identity of a Vessel node (one per machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(Uuid);

impl NodeId {
    /// Generate a fresh random id.
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for NodeId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s.trim()).map(Self)
    }
}

/// Identity of a harness session running on a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Generate a fresh random id.
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

/// Static facts about a node, returned by `GET /node`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Stable id for this node.
    pub id: NodeId,
    /// Human-readable name (defaults to the hostname).
    pub name: String,
    /// OS hostname.
    pub hostname: String,
    /// `std::env::consts::OS`.
    pub os: String,
    /// `std::env::consts::ARCH`.
    pub arch: String,
    /// Crate version of the node binary.
    pub version: String,
    /// Wire protocol version the node speaks.
    pub protocol: u32,
    /// When the node process started.
    pub started_at: DateTime<Utc>,
}

/// An agent harness Vessel knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    /// The `claude` CLI (Claude Code).
    ClaudeCode,
    /// The `codex` CLI.
    Codex,
    /// The `gemini` CLI.
    Gemini,
    /// The `opencode` CLI.
    OpenCode,
}

impl HarnessKind {
    /// Every harness kind, in display order.
    pub const ALL: [Self; 4] = [Self::ClaudeCode, Self::Codex, Self::Gemini, Self::OpenCode];

    /// Executable name to look up on `PATH`.
    #[must_use]
    pub const fn binary(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
        }
    }

    /// Canonical kebab-case name used on the wire and in the CLI.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
        }
    }
}

impl fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error returned when a harness name is not recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownHarness(pub String);

impl fmt::Display for UnknownHarness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown harness `{}`", self.0)
    }
}

impl std::error::Error for UnknownHarness {}

impl FromStr for HarnessKind {
    type Err = UnknownHarness;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claudecode" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            "opencode" | "open-code" => Ok(Self::OpenCode),
            other => Err(UnknownHarness(other.to_owned())),
        }
    }
}

/// A harness that was found installed on a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Harness {
    /// Which harness.
    pub kind: HarnessKind,
    /// Resolved executable path.
    pub path: PathBuf,
    /// First line of `<binary> --version`, if it answered.
    pub version: Option<String>,
}

/// What to run. Sent as the body of `POST /sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    /// Harness to launch.
    pub harness: HarnessKind,
    /// Working directory. Defaults to the node's current directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Extra arguments passed verbatim to the harness.
    #[serde(default)]
    pub args: Vec<String>,
}

/// Lifecycle state of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum SessionStatus {
    /// Process is being spawned.
    Starting,
    /// Process is alive.
    Running,
    /// Process exited on its own or was killed.
    Exited {
        /// Exit code, if the OS reported one.
        code: Option<i32>,
    },
    /// Process could not be started.
    Failed {
        /// Why.
        reason: String,
    },
}

impl SessionStatus {
    /// Whether the session can still receive input.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

/// A session as reported by the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Session id.
    pub id: SessionId,
    /// What was asked for.
    pub spec: SessionSpec,
    /// Current state.
    pub status: SessionStatus,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
}

/// Which output stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One line of harness output, streamed over `GET /sessions/{id}/stream`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChunk {
    /// Session that produced the line.
    pub session: SessionId,
    /// Which stream.
    pub stream: StreamKind,
    /// The line, without its trailing newline.
    pub line: String,
}

/// One message on the `GET /sessions/{id}/stream` websocket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SessionEvent {
    /// A line of output.
    Output(OutputChunk),
    /// The session changed state. Sent last when the session ends.
    Status {
        /// New state.
        status: SessionStatus,
    },
}

/// Body of `POST /sessions/{id}/input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInput {
    /// Text written to the harness's stdin verbatim.
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_kind_round_trips_through_name() {
        for kind in HarnessKind::ALL {
            assert_eq!(kind.name().parse::<HarnessKind>().unwrap(), kind);
            assert_eq!(kind.binary().parse::<HarnessKind>().unwrap(), kind);
        }
    }

    #[test]
    fn session_status_serialises_with_tag() {
        let json = serde_json::to_string(&SessionStatus::Exited { code: Some(0) }).unwrap();
        assert_eq!(json, r#"{"state":"exited","code":0}"#);
    }
}
