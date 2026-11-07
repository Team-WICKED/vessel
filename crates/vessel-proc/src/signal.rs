//! The signals Vessel sends to stop processes.

/// How firmly to ask a process group to stop.
///
/// Only the three stop requests the runtime uses; anything a user wants to
/// send interactively (Ctrl-C) travels as terminal input instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// `SIGHUP`: "your terminal went away". The polite stop for programs in
    /// a PTY; TUIs save state and exit on it.
    Hangup,
    /// `SIGTERM`: the polite stop for programs on pipes.
    Terminate,
    /// `SIGKILL`: cannot be caught; used after the grace period and to sweep
    /// a group whose leader has exited.
    Kill,
}

#[cfg(unix)]
impl Signal {
    pub(crate) const fn to_nix(self) -> nix::sys::signal::Signal {
        match self {
            Self::Hangup => nix::sys::signal::Signal::SIGHUP,
            Self::Terminate => nix::sys::signal::Signal::SIGTERM,
            Self::Kill => nix::sys::signal::Signal::SIGKILL,
        }
    }
}
