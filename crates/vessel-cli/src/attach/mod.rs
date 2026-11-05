//! `vessel attach`: join a running (or finished) session.
//!
//! A terminal session is a byte pipe to the harness's own TUI; a structured
//! session is a conversation. Both replay history from `--from` first (a
//! TUI needs its earlier output to draw the current screen), and both
//! leave the session running when they detach.

mod structured;
pub(crate) mod terminal;

use vessel_client::{CallError, NodeClient};
use vessel_proto::{EventSeq, SessionId, SessionInfo, SessionInput, SessionMode};

use crate::error::CliError;
use crate::io;
use crate::render;

/// Attach to `session` until it ends or the user detaches.
pub(crate) async fn attach(
    client: &NodeClient,
    session: &SessionInfo,
    from: EventSeq,
) -> Result<(), CliError> {
    let ended = match session.spec.mode {
        SessionMode::Terminal { .. } => terminal::attach(client, session.id, from).await?,
        SessionMode::Structured { .. } => structured::attach(client, session.id, from).await?,
    };
    let message = match ended {
        Ended::Detached => format!("[vessel] detached from {}\n", session.id),
        Ended::Session(Some(status)) => format!("[vessel] session {}\n", render::status(&status)),
        Ended::Session(None) => "[vessel] session ended\n".to_owned(),
    };
    io::note(&message).await?;
    Ok(())
}

/// How an attach finished.
#[derive(Debug)]
enum Ended {
    /// The user detached; the session goes on.
    Detached,
    /// The session ended, with its final status if it was seen.
    Session(Option<vessel_proto::SessionStatus>),
}

/// Send input while attached. A refusal from the node (the session just
/// ended, or another client answered a permission request first) is shown
/// and attaching goes on; the timeline says what happened. Losing the
/// connection ends the attach.
async fn send(
    client: &NodeClient,
    session: SessionId,
    input: SessionInput,
) -> Result<(), CliError> {
    match client.send(session, input).await {
        Ok(()) => Ok(()),
        Err(CallError::Remote(error)) => {
            io::note(&format!("\r\n[vessel] {}\r\n", error.message)).await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
