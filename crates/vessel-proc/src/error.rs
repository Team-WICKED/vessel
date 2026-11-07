//! Errors of this crate.

use std::fmt;
use std::path::PathBuf;

/// One of a child's standard streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    /// Standard input.
    Stdin,
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl fmt::Display for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdin => "stdin",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

/// A command could not be started.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The OS refused to start the program.
    #[error("cannot start {}", program.display())]
    Start {
        /// The program.
        program: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The child started without one of its piped streams.
    #[error("{} started without a {stream} pipe", program.display())]
    MissingPipe {
        /// The program.
        program: PathBuf,
        /// Which stream.
        stream: Stream,
    },
    /// The child started but has no pid (it was already reaped).
    #[error("{} started but has no process id", program.display())]
    NoPid {
        /// The program.
        program: PathBuf,
    },
    /// The child's pid cannot name its process group.
    #[error("{} started with a pid that cannot lead a process group", program.display())]
    NotLeader {
        /// The program.
        program: PathBuf,
        /// The rejected pid.
        #[source]
        source: InvalidGroup,
    },
}

/// A stop signal could not be delivered.
#[derive(Debug, thiserror::Error)]
#[error("cannot signal process group")]
pub struct SignalError(#[from] std::io::Error);

impl SignalError {
    /// The OS refused (`EPERM`): a member runs as another user, or (macOS)
    /// every member has begun exiting and is not reaped yet. Which one is
    /// told apart by `ProcessGroup::signal_with_leader`, the one rule every
    /// group signal of a leader that may be exiting goes through.
    #[must_use]
    pub fn is_refused(&self) -> bool {
        self.0.kind() == std::io::ErrorKind::PermissionDenied
    }
}

/// Waiting for a child failed.
#[derive(Debug, thiserror::Error)]
#[error("cannot wait for child process")]
pub struct WaitError(#[from] std::io::Error);

/// A pid that cannot name a child's process group (see
/// `ProcessGroup::of_leader`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("pid {pid} cannot be a child's process group")]
pub struct InvalidGroup {
    /// The rejected pid.
    pub pid: u32,
}
