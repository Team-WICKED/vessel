//! [`ProcessGroup`]: signalling every process in a Unix process group.

use std::time::Duration;

use nix::errno::Errno;
use nix::unistd::Pid;

use crate::error::{InvalidGroup, SignalError};
use crate::signal::Signal;

/// How long a refused group signal waits for the group's leader to be
/// reaped ([`ProcessGroup::signal_with_leader`]). A leader is reaped as soon
/// as it has exited, so this only bounds a real refusal's answer.
pub const REAP_GRACE: Duration = Duration::from_millis(500);

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

    /// Send `signal` to the group of a leader of ours that may be exiting:
    /// the one rule for a refused signal (`EPERM`), shared by
    /// [`Child`](crate::Child) and `vessel-pty`'s killer. On `EPERM`,
    /// `reaped` waits up to [`REAP_GRACE`] for the leader to be reaped (by
    /// the caller's own means) and says whether it was. If it was, the group
    /// is signalled again, with `SIGKILL` as a sweep is: success or `ESRCH`
    /// means everything is gone, and another `EPERM` is a real refusal and
    /// is returned. If it was not, the first refusal is returned.
    ///
    /// [Decision Log]
    /// - Problem: macOS refuses to signal a group whose members have all
    ///   begun exiting (zombies, or on their way to being one), where Linux
    ///   signals zombies without complaint. For our own leader that means it
    ///   is gone. But `EPERM` also means a member runs as another user (a
    ///   setuid helper, `sudo`): a real refusal, which must not pass as
    ///   success.
    /// - One look was not enough: CI on macOS saw `EPERM` while `try_wait`
    ///   still said the leader runs (it had begun exiting and was not yet
    ///   waitable), so the refusal was returned and a session's stop failed
    ///   (2026-11-06, `a_broken_transport_still_settles_the_turn_and_its_
    ///   requests`). Hence a grace for the reap, not one look.
    /// - One function: `vessel-proc` reaps with `try_wait`, `vessel-pty`'s
    ///   waiter thread reaps and reports through a condition variable; both
    ///   say "reaped within the grace" through `reaped`, and the rule after
    ///   it is this one.
    /// - Cost: the caller blocks up to the grace, and only after `EPERM`.
    ///   The signal after a reap may, in the microseconds after the group's
    ///   last member went, reach a group that reused the id: the race every
    ///   sweep after a reap accepts.
    ///
    /// # Errors
    /// The refusal, if the leader was not reaped within the grace or the
    /// group refused again after it was; any other failure to signal.
    pub fn signal_with_leader(
        self,
        signal: Signal,
        reaped: impl FnOnce(Duration) -> bool,
    ) -> Result<(), SignalError> {
        match self.signal(signal) {
            Err(refused) if refused.is_refused() => {
                if reaped(REAP_GRACE) {
                    self.signal(Signal::Kill)
                } else {
                    Err(refused)
                }
            }
            delivered => delivered,
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
