//! What every session actor does the same way: publish while staying
//! stoppable, and stop its process in two steps.
//!
//! The actors differ in what they read (PTY bytes, ACP outputs) and how they
//! decide the final status; both keep their process and a [`Lifecycle`],
//! and are generic over [`Stoppable`] (two implementations: the PTY program
//! and the ACP agent), so there is no dynamic dispatch.

use std::time::Duration;

use tokio::time::Instant;
use vessel_agent::AgentProcess;
use vessel_proto::{AgentEvent, SessionEvent, SessionStatus};
use vessel_pty::{PtyProcess, Signal};

use crate::publish::Publisher;
use crate::stop::{Step, Stopper};

/// How long output may keep arriving after the program exited.
///
/// Normally the output ends right after the exit (the group is swept and
/// nothing holds the PTY or pipes any more). A process that left the group
/// (for example a daemon that called `setsid`) can keep them open forever;
/// the session must still end, so draining gives up after this long.
pub(crate) const DRAIN: Duration = Duration::from_secs(2);

/// The deadline for draining, starting now.
pub(crate) fn drain_deadline() -> Instant {
    Instant::now()
        .checked_add(DRAIN)
        .unwrap_or_else(Instant::now)
}

/// A process a session actor stops in two steps.
pub(crate) trait Stoppable {
    /// Carry out `step` (log, do not fail: the actor keeps going).
    fn stop(&mut self, step: Step);
}

impl Stoppable for PtyProcess {
    /// `SIGHUP` first, which is what a closing terminal sends, then
    /// `SIGKILL` for the whole group.
    fn stop(&mut self, step: Step) {
        let signal = match step {
            Step::Ask => Signal::Hangup,
            Step::Force => Signal::Kill,
        };
        if let Err(error) = self.signal(signal) {
            tracing::warn!(pid = self.pid(), %error, ?signal, "cannot signal terminal program");
        }
    }
}

impl Stoppable for AgentProcess {
    /// `SIGTERM` first, then `SIGKILL` for the whole group.
    fn stop(&mut self, step: Step) {
        let signal = match step {
            Step::Ask => Signal::Terminate,
            Step::Force => Signal::Kill,
        };
        if let Err(error) = self.signal(signal) {
            tracing::warn!(pid = self.pid(), %error, ?signal, "cannot signal agent");
        }
    }
}

/// The publisher and the stop state of one session.
#[derive(Debug)]
pub(crate) struct Lifecycle {
    pub(crate) out: Publisher,
    pub(crate) stop: Stopper,
    /// Vessel's own lines for the log, published right after `Starting`
    /// (`LaunchOptions::notices`).
    pub(crate) notices: Vec<String>,
}

impl Lifecycle {
    /// The first records: `Status{Starting}`, then the notices, each a
    /// `HarnessLog` line.
    pub(crate) async fn starting<P: Stoppable>(&mut self, process: &mut P) {
        self.status(process, SessionStatus::Starting).await;
        let lines = std::mem::take(&mut self.notices);
        self.notices(process, lines).await;
    }

    /// Publish Vessel's own lines, each a `HarnessLog` record.
    pub(crate) async fn notices<P: Stoppable>(&mut self, process: &mut P, lines: Vec<String>) {
        for line in lines {
            self.publish(
                process,
                SessionEvent::Agent {
                    event: AgentEvent::HarnessLog { line },
                },
            )
            .await;
        }
    }

    /// Publish one event, staying responsive to `kill` while the durable
    /// channel is full.
    pub(crate) async fn publish<P: Stoppable>(&mut self, process: &mut P, event: SessionEvent) {
        let slot = loop {
            tokio::select! {
                biased;
                step = self.stop.next() => self.apply(process, step),
                slot = self.out.slot() => break slot,
            }
        };
        self.out.commit(slot, event);
    }

    /// Publish a status change.
    pub(crate) async fn status<P: Stoppable>(&mut self, process: &mut P, status: SessionStatus) {
        self.publish(process, SessionEvent::Status { status }).await;
    }

    /// Carry out a stop step and remember it.
    pub(crate) fn apply<P: Stoppable>(&mut self, process: &mut P, step: Step) {
        process.stop(step);
        self.stop.advance(step);
    }

    /// Start stopping without a `kill` (the session cannot continue), unless
    /// a stop is already under way.
    pub(crate) fn begin_stop<P: Stoppable>(&mut self, process: &mut P) {
        if self.stop.is_idle() {
            self.apply(process, Step::Ask);
        }
    }
}
