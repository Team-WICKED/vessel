//! Structured attach: the agent's timeline as text, typed lines as prompts,
//! numbers as permission answers. Other clients may answer the same
//! request first; the node then refuses ours and the timeline shows who won.
//!
//! History is replayed up to where the session was when attaching before
//! any input is read, so every turn seen afterwards is new. That makes the
//! end of input well defined: the attach leaves once every prompt it sent
//! has started and the turn in progress has ended, so
//! `echo "fix the build" | vessel attach ID` prints the whole reply and
//! exits.

use vessel_proto::{
    AgentEvent, ContentBlock, EventRecord, PermissionOutcome, SessionEvent, SessionInfo,
    SessionInput, SessionStatus,
};

use super::{Ended, Sent, Stream, notice, send};
use crate::error::CliError;
use crate::io;
use crate::view::{Action, Conversation, HELP};

/// Turns as seen by this attach.
#[derive(Debug, Default)]
struct Turns {
    /// A turn has started and not ended.
    in_progress: bool,
    /// Prompts this attach sent whose turn has not started yet.
    unstarted: usize,
    /// Input ended; leave once the turns above are done.
    input_closed: bool,
}

impl Turns {
    fn observe(&mut self, record: &EventRecord) {
        if let SessionEvent::Agent { event } = &record.event {
            match event {
                AgentEvent::TurnStarted { .. } => {
                    self.in_progress = true;
                    self.unstarted = self.unstarted.saturating_sub(1);
                }
                AgentEvent::TurnEnded { .. } | AgentEvent::TurnFailed { .. } => {
                    self.in_progress = false;
                }
                AgentEvent::Ready { .. }
                | AgentEvent::UserMessageChunk { .. }
                | AgentEvent::MessageChunk { .. }
                | AgentEvent::ThoughtChunk { .. }
                | AgentEvent::ToolCall(_)
                | AgentEvent::ToolCallUpdate(_)
                | AgentEvent::Plan { .. }
                | AgentEvent::PermissionRequested(_)
                | AgentEvent::PermissionResolved { .. }
                | AgentEvent::HarnessLog { .. } => {}
            }
        }
    }

    const fn done(&self) -> bool {
        self.input_closed && self.unstarted == 0 && !self.in_progress
    }
}

pub(super) async fn attach(mut stream: Stream, session: &SessionInfo) -> Result<Ended, CliError> {
    let clients = stream.client();
    let mut view = View::default();
    // History first, up to where the session was when it was looked up.
    let joined_at = session.next_seq;
    while stream.cursor() < joined_at {
        if let Some(ended) = view.next(&mut stream).await? {
            return Ok(ended);
        }
    }
    io::note(HELP).await?;
    let mut lines = io::input_lines()?;
    loop {
        tokio::select! {
            ended = view.next(&mut stream) => {
                if let Some(ended) = ended? {
                    return Ok(ended);
                }
                if view.turns.done() {
                    return Ok(Ended::Detached);
                }
            }
            line = lines.recv(), if !view.turns.input_closed => {
                let Some(line) = line else {
                    view.turns.input_closed = true;
                    if view.turns.done() {
                        return Ok(Ended::Detached);
                    }
                    continue;
                };
                let input = match view.conversation.interpret(&line) {
                    Action::Prompt(text) => SessionInput::Prompt {
                        content: vec![ContentBlock::Text { text }],
                    },
                    Action::Cancel => SessionInput::CancelTurn,
                    Action::Answer { request, option } => SessionInput::AnswerPermission {
                        id: request,
                        outcome: PermissionOutcome::Selected { option },
                    },
                    Action::Detach => return Ok(Ended::Detached),
                    Action::Explain(text) => {
                        io::note(&text).await?;
                        continue;
                    }
                    Action::Nothing => continue,
                };
                let is_prompt = matches!(input, SessionInput::Prompt { .. });
                // While reconnecting there is no connection to send on; the
                // line is dropped, not queued, since what it answers may
                // have changed by the time the connection is back.
                let Some(client) = clients.borrow().clone() else {
                    io::note("[vessel] not connected right now; that line was not sent\n").await?;
                    continue;
                };
                if send(&client, session.id, input).await? == Sent::Accepted && is_prompt {
                    view.turns.unstarted = view.turns.unstarted.saturating_add(1);
                }
            }
        }
    }
}

/// What has been shown so far.
#[derive(Debug, Default)]
struct View {
    conversation: Conversation,
    turns: Turns,
    last_status: Option<SessionStatus>,
}

impl View {
    /// Show the next record; `Some` when the session has ended.
    async fn next(&mut self, stream: &mut Stream) -> Result<Option<Ended>, CliError> {
        match stream.next().await {
            None => Ok(Some(Ended::Session(self.last_status.take()))),
            Some(Ok(record)) => {
                if let SessionEvent::Status { status } = &record.event
                    && !status.is_live()
                {
                    self.last_status = Some(status.clone());
                }
                self.turns.observe(&record);
                io::out(self.conversation.show(&record).as_bytes()).await?;
                Ok(None)
            }
            Some(Err(error)) if !error.is_fatal() => {
                notice(&error, "\n").await?;
                Ok(None)
            }
            Some(Err(error)) => Err(error.into()),
        }
    }
}
