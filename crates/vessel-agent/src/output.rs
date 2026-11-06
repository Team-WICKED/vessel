//! [`AgentOutput`] and [`AgentOutputs`]: what an agent session produces.

use tokio::sync::mpsc;
use vessel_proto::AgentEvent;

use crate::error::Disconnect;

/// One output of an agent session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOutput {
    /// A timeline event. `AgentEvent::Ready` marks the end of the
    /// handshake; prompts are accepted from then on.
    Event(AgentEvent),
    /// The ACP connection ended; the driver sends nothing after this.
    /// Harness log lines may still follow until the process's stderr ends.
    Disconnected(Disconnect),
}

/// The outputs of an agent session, in order.
///
/// Bounded: when the consumer stops reading, the driver stops, its
/// transport stops reading the agent's stdout, and the agent blocks on its
/// writes. Nothing is dropped. (What the SDK reads regardless is listed in
/// the `transport` module and ADR 0003.)
#[derive(Debug)]
pub struct AgentOutputs(mpsc::Receiver<AgentOutput>);

impl AgentOutputs {
    pub(crate) const fn new(receiver: mpsc::Receiver<AgentOutput>) -> Self {
        Self(receiver)
    }

    /// The next output, or `None` once the driver and the stderr pump have
    /// both finished. Cancel-safe.
    pub async fn next(&mut self) -> Option<AgentOutput> {
        self.0.recv().await
    }
}
