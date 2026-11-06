//! Exit codes of `vesseld attach`, the SSH-side proxy (ADR 0007).
//!
//! [Decision Log]
//! - Purpose: a client that ran `ssh host vesseld attach` learns why the
//!   proxy ended only from its exit status; there is no frame left to carry
//!   an error once the byte stream is gone.
//! - Alternatives: a final error frame (lost whenever the link itself is the
//!   failure); parsing stderr text (untyped, locale-dependent).
//! - Choice: a closed enum of codes in a range nothing else uses. Rust uses
//!   101 for panics, clap uses 2, shells use 126/127 and 128+n for signals,
//!   and `ssh` uses 255; 80-82 collide with none of them.
//! - Constraint: codes are part of the protocol. Never renumber one; retire
//!   it and pick a new number instead.

/// Why `vesseld attach` exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProxyExit {
    /// The client closed the stream normally.
    Closed,
    /// No node answered on the local socket and starting one failed.
    NodeUnavailable,
    /// The data directory or token is unreadable, or the directory is not
    /// private.
    DataDirUnusable,
    /// The node at the socket refused the token.
    Refused,
}

impl ProxyExit {
    /// Every variant, for exhaustive tests.
    pub const ALL: [Self; 4] = [
        Self::Closed,
        Self::NodeUnavailable,
        Self::DataDirUnusable,
        Self::Refused,
    ];

    /// The process exit code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Closed => 0,
            Self::NodeUnavailable => 80,
            Self::DataDirUnusable => 81,
            Self::Refused => 82,
        }
    }

    /// The variant for an exit code, if it is one of ours. `127` (the remote
    /// shell's "command not found") is deliberately not ours: clients read it
    /// as "not installed".
    #[must_use]
    pub const fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::Closed),
            80 => Some(Self::NodeUnavailable),
            81 => Some(Self::DataDirUnusable),
            82 => Some(Self::Refused),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_avoid_reserved_ranges() {
        for exit in ProxyExit::ALL {
            assert_eq!(ProxyExit::from_code(i32::from(exit.code())), Some(exit));
            assert!(!matches!(exit.code(), 1 | 2 | 101 | 126..=255));
        }
        assert_eq!(ProxyExit::from_code(127), None);
    }
}
