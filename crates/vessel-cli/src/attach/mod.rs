//! `vessel attach`: join a running (or finished) session.
//!
//! A terminal session is a byte pipe to the harness's own TUI; a structured
//! session is a conversation. Both replay history from `--from` first (a
//! TUI needs its earlier output to draw the current screen), and both
//! leave the session running when they detach.
//!
//! Both survive a lost connection (milestone 2): they follow the session
//! through a `ResumingStream`, which dials the node again and continues
//! from its cursor, so nothing is shown twice or skipped. The user sees
//! `[vessel] connection lost; reconnecting…` and `[vessel] reconnected`.
//! Input goes to whichever connection is current; input in flight when the
//! connection dropped is reported as possibly not applied and is never
//! sent again.

mod structured;
pub(crate) mod terminal;

use std::sync::Arc;

use vessel_client::{CallError, NodeClient, ResumingStream, StreamError, follow_resuming};
use vessel_proto::{EventSeq, SessionId, SessionInfo, SessionInput, SessionMode};

use crate::connect::{Connected, NodeConnector, client_info};
use crate::error::CliError;
use crate::io;
use crate::render;

/// Attach to `session` until it ends or the user detaches.
pub(crate) async fn attach(
    connected: &Connected,
    session: &SessionInfo,
    from: EventSeq,
) -> Result<(), CliError> {
    let stream = follow_resuming(
        Arc::clone(&connected.connector),
        client_info(),
        session.id,
        from,
    )
    .reusing(connected.client.clone());
    let ended = match session.spec.mode {
        SessionMode::Terminal { .. } => terminal::attach(stream, session.id).await?,
        SessionMode::Structured { .. } => structured::attach(stream, session).await?,
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

/// The follow both attach modes read.
type Stream = ResumingStream<Arc<NodeConnector>>;

/// What became of input sent while attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    /// The node took it.
    Accepted,
    /// The node refused it (shown).
    Refused,
    /// The connection dropped during the send (shown): it may or may not
    /// have arrived.
    Lost,
}

/// Send input while attached.
///
/// - A refusal (the session just ended, or another client answered a
///   permission request first) is shown and attaching goes on; the
///   timeline says what happened.
/// - A connection lost during the send is shown as such: the input may or
///   may not have arrived, and it is not sent again (milestone 2 plan,
///   C1). Attaching goes on; the stream reconnects.
async fn send(
    client: &NodeClient,
    session: SessionId,
    input: SessionInput,
) -> Result<Sent, CliError> {
    match client.send(session, input).await {
        Ok(()) => Ok(Sent::Accepted),
        Err(CallError::Remote(error)) => {
            io::note(&format!("\r\n[vessel] {}\r\n", error.message)).await?;
            Ok(Sent::Refused)
        }
        Err(CallError::Disconnected) => {
            io::note(
                "\r\n[vessel] the connection dropped while sending input; \
                 it may or may not have been applied\r\n",
            )
            .await?;
            Ok(Sent::Lost)
        }
        Err(error) => Err(error.into()),
    }
}

/// Show a non-fatal stream notice: the first loss, every fifth attempt
/// after it (so a long outage is visibly still being retried), the
/// resume, and pruned records.
async fn notice(error: &StreamError, line_end: &str) -> Result<(), CliError> {
    let show = match error {
        StreamError::Reconnecting { attempt, .. } => *attempt == 1 || attempt % 5 == 0,
        StreamError::Resumed { .. } => {
            io::note(&format!("{line_end}[vessel] reconnected{line_end}")).await?;
            return Ok(());
        }
        StreamError::Missing { .. }
        | StreamError::Call(_)
        | StreamError::Unsubscribed
        | StreamError::Unreachable { .. } => true,
    };
    if show {
        io::note(&format!("{line_end}[vessel] {error}{line_end}")).await?;
    } else {
        tracing::debug!(%error, "still reconnecting");
    }
    Ok(())
}
