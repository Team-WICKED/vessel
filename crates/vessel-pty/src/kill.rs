//! [`Killer`]: how the program in a PTY is stopped on each platform.

use vessel_proc::{InvalidGroup, Signal, SignalError};

/// Stops the program running in a PTY.
///
/// Unix: the program is a session leader, so its process group holds it and
/// everything it started; signals go to the group. Windows: `portable-pty`'s
/// killer terminates the program itself (any signal means "terminate");
/// processes it started end when the pseudo console closes.
#[derive(Debug)]
pub(crate) struct Killer {
    #[cfg(unix)]
    group: vessel_proc::ProcessGroup,
    /// `portable-pty`'s own trait object; holding it is the only way to
    /// terminate a `ConPTY` child from outside its waiting thread.
    #[cfg(not(unix))]
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

impl Killer {
    #[cfg(unix)]
    pub(crate) fn new(
        _child: &(dyn portable_pty::Child + Send + Sync),
        pid: u32,
    ) -> Result<Self, InvalidGroup> {
        Ok(Self {
            group: vessel_proc::ProcessGroup::of_leader(pid)?,
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

    /// Ask the program (Unix: its whole group) to stop.
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
            self.group.signal(signal)
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

    /// A handle for the waiter thread to sweep the group after the leader
    /// exits (Unix only; see the crate docs).
    #[cfg(unix)]
    pub(crate) const fn group(&self) -> vessel_proc::ProcessGroup {
        self.group
    }
}
