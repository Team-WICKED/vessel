//! Structured attach: the agent's timeline as text, typed lines as prompts,
//! numbers as permission answers. Other clients may answer the same
//! request first; the node then refuses ours and shows who won through the
//! timeline.

use vessel_client::NodeClient;
use vessel_proto::{
    ContentBlock, EventSeq, PermissionOutcome, SessionEvent, SessionId, SessionInput,
};

use super::{Ended, send};
use crate::error::CliError;
use crate::io;
use crate::view::{Action, Conversation, HELP};

pub(super) async fn attach(
    client: &NodeClient,
    session: SessionId,
    from: EventSeq,
) -> Result<Ended, CliError> {
    let mut stream = client.follow(session, from).await?;
    io::note(HELP).await?;
    let mut lines = io::input_lines()?;
    let mut conversation = Conversation::default();
    let mut last_status = None;
    loop {
        tokio::select! {
            item = stream.next() => match item {
                None => return Ok(Ended::Session(last_status)),
                Some(Ok(record)) => {
                    if let SessionEvent::Status { status } = &record.event
                        && !status.is_live()
                    {
                        last_status = Some(status.clone());
                    }
                    io::out(conversation.show(&record).as_bytes()).await?;
                }
                Some(Err(error)) if !error.is_fatal() => {
                    io::note(&format!("[vessel] {error}\n")).await?;
                }
                Some(Err(error)) => return Err(error.into()),
            },
            line = lines.recv() => {
                let Some(line) = line else {
                    return Ok(Ended::Detached);
                };
                let input = match conversation.interpret(&line) {
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
                send(client, session, input).await?;
            }
        }
    }
}
