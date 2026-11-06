//! Google Gemini CLI (`gemini`).
//!
//! Speaks ACP natively. `--experimental-acp` is the flag Gemini CLI has
//! accepted since it gained ACP support; newer releases also accept `--acp`
//! and keep the old spelling as an alias, so the old one works with both.
//!
//! Not installed on the machine the profile work was grounded on
//! (2026-11-06), so only what is documented is used: the home is
//! `~/.gemini` with no variable to move it, there is no login check, its
//! sessions cannot be moved, and terminal sessions get no MCP servers at
//! launch (structured ones still do, over ACP).

use super::{Descriptor, Home, Structured};
use crate::layout::HarnessLayout;

pub(super) const DESCRIPTOR: Descriptor = Descriptor {
    binary: "gemini",
    profile: "gemini",
    structured: Structured::Native {
        args: &["--experimental-acp"],
    },
    home: Home {
        variable: None,
        default: &[".gemini"],
    },
    layout: HarnessLayout {
        instructions: Some("GEMINI.md"),
        skills: None,
    },
    login: None,
    sessions: None,
    terminal_mcp: None,
    terminal_args: true,
};
