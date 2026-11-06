//! [`Killer`] and [`Reaper`]: how the program in a PTY is stopped and
//! reaped on each platform.
//!
//! [Decision Log]
//! - Purpose: on Unix the program leads its own process group, and stopping
//!   it signals the group id (its pid). Once the waiter thread has reaped
//!   the leader and the group's last member is gone, that id is free for
//!   the system to hand out again. The owner learns of the exit only when
//!   it next polls `wait`, and the session's stop escalation (`SIGHUP`, then
//!   `SIGKILL` two seconds later) does not poll first, so a late `SIGKILL`
//!   could reach an unrelated process group that reused the id.
//! - Choice: the waiter and the killer share the group and an "exited"
//!   flag under one lock. Where the platform has `waitid(WNOWAIT)` (Linux,
//!   Android, FreeBSD), the waiter waits for the exit without reaping, so
//!   the zombie leader keeps its id reserved; then, under the lock, it sets
//!   the flag, sweeps the group and reaps. A signal takes the same lock and
//!   does nothing once the flag is set. No signal can reach a reused id,
//!   but for the one after a refused signal's reap (below), in the
//!   microseconds the sweep after any reap has too.
//! - Elsewhere (macOS: `nix` offers `WNOWAIT` but not `waitid` there) the
//!   waiter reaps first and sets the flag right after: the window shrinks
//!   from the escalation's seconds to the microseconds between the two, the
//!   same race `vessel-proc` accepts for its sweep. Closing it there would
//!   take `kqueue`'s `NOTE_EXIT`, which this crate cannot test.
//! - In that window macOS refuses to signal the group (`EPERM`: every
//!   member has exited, none is reaped yet). A refused signal follows the
//!   rule `vessel-proc` keeps for every group signal
//!   (`ProcessGroup::signal_with_leader`): it waits, on a condition
//!   variable and at most `vessel_proc::REAP_GRACE`, for the waiter to set
//!   the flag; if it does, the group is signalled again (gone, or a real
//!   refusal from a member running as another user, which is returned); if
//!   not, the first refusal is returned.
//! - Windows has no process groups: the killer terminates the program and
//!   the pseudo console's closing ends the rest.

#[cfg(unix)]
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use vessel_proc::{InvalidGroup, Signal, SignalError};

/// Stops the program running in a PTY.
#[derive(Debug)]
pub(crate) struct Killer {
    #[cfg(unix)]
    leader: Arc<Leader>,
    /// `portable-pty`'s own trait object; holding it is the only way to
    /// terminate a `ConPTY` child from outside its waiting thread.
    #[cfg(not(unix))]
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

/// The program's process group, and whether its leader has exited (Unix).
#[cfg(unix)]
#[derive(Debug)]
struct Leader {
    group: vessel_proc::ProcessGroup,
    /// Set by the waiter once the leader exited, before it is reaped
    /// (see the module's Decision Log).
    exited: Mutex<bool>,
    /// Woken when `exited` is set.
    exiting: Condvar,
}

#[cfg(unix)]
impl Leader {
    /// Signal the group, unless the leader has exited.
    fn signal(&self, signal: Signal) -> Result<(), SignalError> {
        let exited = self.exited.lock().unwrap_or_else(PoisonError::into_inner);
        if *exited {
            return Ok(());
        }
        self.group.signal_with_leader(signal, move |grace| {
            let (exited, _timeout) = self
                .exiting
                .wait_timeout_while(exited, grace, |exited| !*exited)
                .unwrap_or_else(PoisonError::into_inner);
            *exited
        })
    }

    /// The leader exited: from now on signals do nothing. Sweeps the group
    /// (its leftovers must not keep the PTY open), then runs `reap`, all
    /// under the lock.
    fn exited<T>(&self, reap: impl FnOnce() -> T) -> T {
        let mut exited = self.exited.lock().unwrap_or_else(PoisonError::into_inner);
        *exited = true;
        self.exiting.notify_all();
        if let Err(error) = self.group.signal(Signal::Kill) {
            tracing::warn!(%error, "cannot sweep PTY process group");
        }
        reap()
    }
}

impl Killer {
    #[cfg(unix)]
    pub(crate) fn new(
        _child: &(dyn portable_pty::Child + Send + Sync),
        pid: u32,
    ) -> Result<Self, InvalidGroup> {
        Ok(Self {
            leader: Arc::new(Leader {
                group: vessel_proc::ProcessGroup::of_leader(pid)?,
                exited: Mutex::new(false),
                exiting: Condvar::new(),
            }),
        })
    }

    #[cfg(not(unix))]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "same signature as the Unix version, which validates the pid"
    )]
    pub(crate) fn new(
        child: &(dyn portable_pty::Child + Send + Sync),
        _pid: u32,
    ) -> Result<Self, InvalidGroup> {
        Ok(Self {
            killer: child.clone_killer(),
        })
    }

    /// Ask the program (Unix: its whole group) to stop. Does nothing once
    /// the waiter has seen it exit.
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unnecessary_wraps,
            reason = "same signature as the Unix version, whose signal can fail"
        )
    )]
    pub(crate) fn signal(&mut self, signal: Signal) -> Result<(), SignalError> {
        #[cfg(unix)]
        {
            self.leader.signal(signal)
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            // `portable-pty` 0.9's Windows killer inverts `TerminateProcess`'s
            // result: success comes back as an error carrying "the
            // operation completed successfully", failure as `Ok`. Its value
            // therefore says nothing, and terminating our own child can
            // only fail once the child has exited, which is what stopping
            // wants anyway. So the request counts as delivered either way.
            if let Err(reported) = self.killer.kill() {
                tracing::trace!(%reported, "pty killer result (inverted in portable-pty 0.9)");
            }
            Ok(())
        }
    }

    /// What the waiter thread needs to finish the program.
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_self,
            reason = "on Unix the reaper shares this killer's group and flag"
        )
    )]
    pub(crate) fn reaper(&self, pid: u32) -> Reaper {
        Reaper {
            #[cfg(unix)]
            leader: Arc::clone(&self.leader),
            pid,
        }
    }
}

/// Used by the waiter thread: waits for the program to exit, sweeps its
/// group, and reaps it, in the order the module's Decision Log explains.
#[derive(Debug)]
pub(crate) struct Reaper {
    #[cfg(unix)]
    leader: Arc<Leader>,
    pid: u32,
}

impl Reaper {
    /// Wait for the program to exit, then finish it; `reap` collects its
    /// status (`portable-pty`'s blocking `wait`) and its result is returned.
    pub(crate) fn finish<T>(&self, reap: impl FnOnce() -> T) -> T {
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        {
            if wait_without_reaping(self.pid) {
                return self.leader.exited(reap);
            }
            let reaped = reap();
            self.leader.exited(|| ());
            reaped
        }
        #[cfg(all(
            unix,
            not(any(target_os = "linux", target_os = "android", target_os = "freebsd"))
        ))]
        {
            let _ = self.pid;
            let reaped = reap();
            self.leader.exited(|| ());
            reaped
        }
        #[cfg(not(unix))]
        {
            let _ = self.pid;
            reap()
        }
    }
}

/// Block until `pid` has exited, leaving it unreaped (a zombie that keeps
/// its id). `false` if the system refused, and the caller reaps first.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn wait_without_reaping(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::wait::{Id, WaitPidFlag, waitid};
    use nix::unistd::Pid;

    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    loop {
        match waitid(
            Id::Pid(Pid::from_raw(raw)),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT,
        ) {
            Ok(_) => return true,
            Err(Errno::EINTR) => {}
            Err(error) => {
                tracing::warn!(pid, %error, "cannot wait for the PTY program without reaping it");
                return false;
            }
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::process::Command;

    use super::*;

    /// A child of its own process group, so signalling the group reaches
    /// it and nothing else.
    fn sleeper() -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn signals_do_nothing_once_the_leader_has_exited() {
        let mut child = sleeper();
        let leader = Leader {
            group: vessel_proc::ProcessGroup::of_leader(child.id()).unwrap(),
            exited: Mutex::new(true),
            exiting: Condvar::new(),
        };
        leader.signal(Signal::Kill).unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "the group was signalled"
        );
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    #[test]
    fn waiting_without_reaping_leaves_the_status_to_collect() {
        let mut child = Command::new("true").spawn().unwrap();
        assert!(wait_without_reaping(child.id()));
        // Still a zombie: the status is there to collect, exactly once.
        assert!(child.try_wait().unwrap().unwrap().success());
    }
}
