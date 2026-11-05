//! [`AgentProcess`]: an ACP agent process and the tasks that serve it.

use tokio::sync::mpsc;
use vessel_proc::{CommandSpec, Signal, SignalError, Spawned};

use crate::driver;
use crate::error::AgentError;
use crate::handshake::AgentSetup;
use crate::input::AgentInput;
use crate::output::{AgentOutput, AgentOutputs};
use crate::stderr;

/// Outputs buffered between the driver and the consumer.
const OUTPUTS: usize = 64;

/// Commands queued for the driver. Small: each caller waits for its reply
/// anyway, so a deep queue would only hide a stuck agent.
const COMMANDS: usize = 16;

/// A running ACP agent (a harness or its adapter).
///
/// Owned by one task (the session actor), which stops it and waits for it.
/// The ACP connection runs in a driver task fed by the process's stdio;
/// stderr lines become `HarnessLog` events. Dropping an `AgentProcess`
/// whose process is still running kills its whole process group, which
/// also ends the driver.
#[derive(Debug)]
pub struct AgentProcess {
    child: vessel_proc::Child,
}

impl AgentProcess {
    /// Start `command` as the leader of a new process group and begin the
    /// ACP handshake described by `setup`.
    ///
    /// Must be called inside a Tokio runtime: the driver and the stderr
    /// pump are spawned onto it.
    ///
    /// # Errors
    /// Fails if the program cannot be started. Handshake failures arrive
    /// later, as [`AgentOutput::Disconnected`].
    pub fn spawn(
        command: &CommandSpec,
        setup: AgentSetup,
    ) -> Result<(Self, AgentInput, AgentOutputs), AgentError> {
        let Spawned {
            child,
            stdin,
            stdout,
            stderr,
        } = vessel_proc::spawn(command)?;
        let (outputs, receiver) = mpsc::channel::<AgentOutput>(OUTPUTS);
        let (commands, command_receiver) = mpsc::channel(COMMANDS);
        drop(tokio::spawn(stderr::pump(stderr, outputs.clone())));
        drop(tokio::spawn(driver::run(
            stdin,
            stdout,
            command_receiver,
            outputs,
            setup,
        )));
        tracing::debug!(pid = child.pid(), program = %command.program.display(), "agent spawned");
        Ok((
            Self { child },
            AgentInput::new(commands),
            AgentOutputs::new(receiver),
        ))
    }

    /// The agent's process id (on Unix also its process group id).
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.child.pid()
    }

    /// Ask the agent's process group to stop ([`Signal::Terminate`] is the
    /// polite choice). Does nothing once it has exited.
    ///
    /// # Errors
    /// Fails if the OS refuses to deliver the signal.
    pub fn signal(&mut self, signal: Signal) -> Result<(), SignalError> {
        self.child.signal(signal)
    }

    /// Wait for the agent to exit; returns its exit code, or `None` when it
    /// was ended by a signal or its status is unknown. The rest of its
    /// group is killed once it exits. Cancel-safe.
    pub async fn wait(&mut self) -> Option<i32> {
        match self.child.wait().await {
            Ok(code) => code,
            Err(error) => {
                tracing::warn!(pid = self.child.pid(), %error, "cannot wait for agent");
                None
            }
        }
    }
}
