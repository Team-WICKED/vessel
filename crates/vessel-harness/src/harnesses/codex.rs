//! The `OpenAI` Codex CLI (`codex`).
//!
//! Structured mode goes through Zed's ACP adapter, `codex-acp`, which embeds
//! Codex and reads the same `~/.codex` configuration. The Codex CLI itself
//! speaks its own app-server protocol, not ACP (ADR 0003 explains why Vessel
//! does not implement per-harness protocols).

use super::{Descriptor, Structured};

pub(super) const DESCRIPTOR: Descriptor = Descriptor {
    binary: "codex",
    structured: Structured::Adapter {
        binary: "codex-acp",
        install: "npm install -g @zed-industries/codex-acp",
    },
};
