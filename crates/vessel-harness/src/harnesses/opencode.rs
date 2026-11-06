//! The `OpenCode` CLI (`opencode`).
//!
//! Speaks ACP natively through its `acp` subcommand, which serves ACP on
//! stdio. That subcommand has no model flag; the model is chosen over ACP
//! (see the crate docs).
//!
//! Checked 2026-11-06 against `OpenCode` 1.18.23 (`--help` only): its
//! configuration is `~/.config/opencode`. It reads `OPENCODE_CONFIG`,
//! `OPENCODE_CONFIG_DIR` and `OPENCODE_CONFIG_CONTENT`, but how each
//! combines with the user's own configuration (and whether one moves the
//! logins) was not confirmed, so the home has no variable, terminal
//! sessions get no MCP servers at launch (structured ones still do, over
//! ACP), and there is no login check or session move yet.

use super::{Descriptor, Home, Structured};

pub(super) const DESCRIPTOR: Descriptor = Descriptor {
    binary: "opencode",
    profile: "opencode",
    structured: Structured::Native { args: &["acp"] },
    home: Home {
        variable: None,
        default: &[".config", "opencode"],
    },
    login: None,
    sessions: None,
    terminal_mcp: None,
    terminal_args: true,
};
