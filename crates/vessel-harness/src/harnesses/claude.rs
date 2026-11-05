//! Claude Code (`claude`).
//!
//! Structured mode goes through the ACP project's adapter,
//! `claude-agent-acp` (npm `@agentclientprotocol/claude-agent-acp`), built on
//! the Claude Agent SDK. The SDK runs Claude
//! Code underneath and reads the same `~/.claude` settings, so skills, MCP
//! servers and permissions behave as in the terminal. Claude Code itself does
//! not speak ACP.

use super::{Descriptor, Structured};

pub(super) const DESCRIPTOR: Descriptor = Descriptor {
    binary: "claude",
    structured: Structured::Adapter {
        binary: "claude-agent-acp",
        install: "npm install -g @agentclientprotocol/claude-agent-acp",
    },
};
