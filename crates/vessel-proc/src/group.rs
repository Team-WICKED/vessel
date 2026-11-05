//! [`ProcessGroup`]: signalling every process in a Unix process group.

use nix::errno::Errno;
use nix::unistd::Pid;

use crate::error::{InvalidGroup, SignalError};
use crate::signal::Signal;

/// A Unix process group, identified by its leader's pid.
///
/// Constructing one only accepts pids that can name a child's group: `0`
/// would mean the caller's own group (the node itself), and `1` would make
/// `killpg` signal every process the node may signal. Both are rejected, so
/// a stray value can never take the node or the machine down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessGroup(Pid);

impl ProcessGroup {
    /// The group led by `pid`.
    ///
    /// The process must have been started as its own group leader
    /// (`setpgid(0, 0)` or `setsid`, as [`spawn`](crate::spawn) and
    /// `portable-pty` do). For any other pid the group does not exist and
    /// signalling it is a no-op.
    ///
    /// # Errors
    /// Fails for `0`, `1` and values above `i32::MAX`.
    pub fn of_leader(pid: u32) -> Result<Self, InvalidGroup> {
        match i32::try_from(pid) {
            Ok(raw) if raw > 1 => Ok(Self(Pid::from_raw(raw))),
            Ok(_) | Err(_) => Err(InvalidGroup { pid }),
        }
    }

    /// The group id (the leader's pid).
    #[must_use]
    pub const fn id(self) -> i32 {
        self.0.as_raw()
    }

    /// Send `signal` to every process in the group.
    ///
    /// A group that no longer exists counts as success: stopping something
    /// already gone is not an error.
    ///
    /// # Errors
    /// Fails if the OS refuses (for example a member runs as another user).
    pub fn signal(self, signal: Signal) -> Result<(), SignalError> {
        match nix::sys::signal::killpg(self.0, signal.to_nix()) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(errno) => Err(SignalError::from(std::io::Error::from(errno))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_pids_that_would_hit_the_node_or_everyone() {
        assert!(ProcessGroup::of_leader(0).is_err());
        assert!(ProcessGroup::of_leader(1).is_err());
        assert!(ProcessGroup::of_leader(u32::MAX).is_err());
        assert_eq!(ProcessGroup::of_leader(4242).unwrap().id(), 4242);
    }

    #[test]
    fn signalling_a_vanished_group_succeeds() {
        // Very likely unused; ESRCH is mapped to success either way, and any
        // live group with this id would belong to another user (EPERM).
        let group = ProcessGroup::of_leader(i32::MAX.unsigned_abs()).unwrap();
        assert!(group.signal(Signal::Kill).is_ok());
    }
}
