//! Per-harness knowledge, one module per [`HarnessKind`].
//!
//! [`descriptor`] is the only `match` on `HarnessKind` that decides
//! behaviour in the whole workspace. Adding a harness variant fails to
//! compile here until its module exists.

mod claude;
mod codex;
mod gemini;
mod opencode;

use vessel_proto::HarnessKind;

/// What Vessel needs to know to run one harness.
#[derive(Debug)]
pub(crate) struct Descriptor {
    /// Executable name looked up on the search path (without `.exe`).
    pub(crate) binary: &'static str,
    /// How structured (ACP) mode is started.
    pub(crate) structured: Structured,
}

/// How a harness speaks ACP.
#[derive(Debug)]
pub(crate) enum Structured {
    /// The harness binary itself speaks ACP when given these arguments.
    Native {
        /// Arguments that switch the harness into ACP mode.
        args: &'static [&'static str],
    },
    /// A separate adapter program speaks ACP and drives the harness.
    Adapter {
        /// The adapter's executable name.
        binary: &'static str,
        /// How a user installs it; shown when it is missing.
        install: &'static str,
    },
}

/// The descriptor of `kind`.
pub(crate) const fn descriptor(kind: HarnessKind) -> &'static Descriptor {
    match kind {
        HarnessKind::ClaudeCode => &claude::DESCRIPTOR,
        HarnessKind::Codex => &codex::DESCRIPTOR,
        HarnessKind::Gemini => &gemini::DESCRIPTOR,
        HarnessKind::OpenCode => &opencode::DESCRIPTOR,
    }
}
