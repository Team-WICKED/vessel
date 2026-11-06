//! Signals that end an attach the clean way.
//!
//! In raw mode the terminal sends `Ctrl-C` to the harness as a key, so a
//! stop request can only come from outside: `SIGTERM` (`kill`), `SIGHUP`
//! (the terminal or SSH connection went away) or `SIGINT` (`kill -INT`, or
//! `Ctrl-C` when input is piped). Without a handler each would end the
//! process at once, leaving the terminal in raw mode and in the harness's
//! modes; with one, the attach detaches and its guard restores the terminal.

#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};

/// Resolves on the first stop request.
pub(super) struct Stop {
    #[cfg(unix)]
    terminate: Option<Signal>,
    #[cfg(unix)]
    hangup: Option<Signal>,
    #[cfg(unix)]
    interrupt: Option<Signal>,
    /// Windows: `Ctrl-C` (only with piped input; raw mode reads it as a key).
    #[cfg(windows)]
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
}

impl Stop {
    /// Start listening. A signal that cannot be listened for keeps its
    /// default behaviour (logged).
    #[cfg(unix)]
    pub(super) fn listen() -> Self {
        let listen = |kind: SignalKind| {
            signal(kind)
                .inspect_err(|error| tracing::warn!(%error, ?kind, "cannot handle signal"))
                .ok()
        };
        Self {
            terminate: listen(SignalKind::terminate()),
            hangup: listen(SignalKind::hangup()),
            interrupt: listen(SignalKind::interrupt()),
        }
    }

    /// Start listening; on Windows only `Ctrl-C` (piped input) applies.
    #[cfg(windows)]
    pub(super) fn listen() -> Self {
        Self {
            ctrl_c: tokio::signal::windows::ctrl_c()
                .inspect_err(|error| tracing::warn!(%error, "cannot handle Ctrl-C"))
                .ok(),
        }
    }

    /// Wait for a stop request.
    #[cfg(unix)]
    pub(super) async fn requested(&mut self) {
        tokio::select! {
            () = next(self.terminate.as_mut()) => {}
            () = next(self.hangup.as_mut()) => {}
            () = next(self.interrupt.as_mut()) => {}
        }
    }

    /// Wait for a stop request.
    #[cfg(windows)]
    pub(super) async fn requested(&mut self) {
        match self.ctrl_c.as_mut() {
            Some(ctrl_c) => {
                if ctrl_c.recv().await.is_none() {
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending::<()>().await,
        }
    }
}

/// The next delivery of `signal`; never, if it is not being listened for.
#[cfg(unix)]
async fn next(signal: Option<&mut Signal>) {
    match signal {
        Some(signal) => {
            if signal.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}
