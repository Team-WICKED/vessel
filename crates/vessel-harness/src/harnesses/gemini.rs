//! Google Gemini CLI (`gemini`).
//!
//! Speaks ACP natively. `--experimental-acp` is the flag Gemini CLI has
//! accepted since it gained ACP support; newer releases also accept `--acp`
//! and keep the old spelling as an alias, so the old one works with both.

use super::{Descriptor, Structured};

pub(super) const DESCRIPTOR: Descriptor = Descriptor {
    binary: "gemini",
    structured: Structured::Native {
        args: &["--experimental-acp"],
    },
};
