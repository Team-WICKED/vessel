//! Errors of this crate.

use std::path::PathBuf;

use vessel_proto::HarnessKind;

/// A session could not be turned into a command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HarnessError {
    /// The harness binary is not on the search path.
    #[error("{harness} is not installed: `{binary}` was not found")]
    NotInstalled {
        /// Which harness.
        harness: HarnessKind,
        /// The binary that was looked for.
        binary: &'static str,
    },
    /// Structured mode cannot run on this node (for example its ACP adapter
    /// is missing).
    #[error("{harness} cannot run in structured mode: {reason}")]
    StructuredUnavailable {
        /// Which harness.
        harness: HarnessKind,
        /// What is missing and how to fix it.
        reason: String,
    },
    /// The requested working directory is not a directory.
    #[error("working directory {} does not exist or is not a directory", cwd.display())]
    NoSuchDirectory {
        /// The rejected directory.
        cwd: PathBuf,
    },
}
