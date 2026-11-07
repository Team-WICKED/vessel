//! Which agent harnesses exist and what a node found installed.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An agent harness Vessel knows how to drive.
///
/// Closed on purpose: everything harness-specific (argv, environment,
/// structured-mode launch) lives in one module per variant in
/// `vessel-harness`, and adding a variant must fail to compile until that
/// module exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    /// Claude Code (`claude`).
    ClaudeCode,
    /// The `OpenAI` Codex CLI (`codex`).
    Codex,
    /// Google Gemini CLI (`gemini`).
    Gemini,
    /// The `OpenCode` CLI (`opencode`).
    OpenCode,
}

impl HarnessKind {
    /// Every harness kind, in display order.
    pub const ALL: [Self; 4] = [Self::ClaudeCode, Self::Codex, Self::Gemini, Self::OpenCode];

    /// Canonical kebab-case name used on the wire and in the CLI.
    #[must_use]
    #[cfg_attr(
        feature = "ts",
        expect(
            clippy::same_name_method,
            reason = "`ts_rs::TS::name` is the type's TypeScript name, an associated function; callers of this method have a value"
        )
    )]
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

/// A harness name that is not recognised.
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

    /// Accepts the canonical name and the binary name (`claude`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            "opencode" => Ok(Self::OpenCode),
            other => Err(UnknownHarness(other.to_owned())),
        }
    }
}

/// Whether a harness can run as a structured (ACP) session on this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StructuredSupport {
    /// Ready: the harness speaks ACP itself, or its adapter is installed.
    Available,
    /// Not usable; `reason` says what is missing (for example the adapter).
    Unavailable {
        /// Human-readable reason.
        reason: String,
    },
}

/// A harness found on a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Harness {
    /// Which harness.
    pub kind: HarnessKind,
    /// Resolved executable path.
    pub path: PathBuf,
    /// First line of `<binary> --version`, if it answered in time.
    pub version: Option<String>,
    /// Structured-mode availability. Terminal mode is always available for an
    /// installed harness.
    pub structured: StructuredSupport,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for kind in HarnessKind::ALL {
            assert_eq!(kind.name().parse::<HarnessKind>().unwrap(), kind);
        }
        assert_eq!(
            "claude".parse::<HarnessKind>().unwrap(),
            HarnessKind::ClaudeCode
        );
    }
}
