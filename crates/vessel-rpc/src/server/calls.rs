//! The reading half of a server connection: decodes client frames, runs each
//! call as its own task, and hands every answer to the outbound queue.
//!
//! Calls run as tasks (not as futures polled by this reader) so a slow call
//! never delays reading the next frame, `Cancel` can abort exactly one call,
//! and a handler that fails unexpectedly takes down only its own call: the
//! client still gets an `Internal` answer for it.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::num::NonZeroUsize;
use std::sync::Arc;

use futures_util::{Stream, StreamExt as _};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use vessel_proto::{
    Call, ClientFrame, ErrorCode, Handler, Outcome, RequestId, WireError, dispatch,
};

use super::outbound::Outgoing;
use crate::error::{ProtocolViolation, ServeError};

/// The calls in flight on one connection.
pub(super) struct Calls<H: Handler> {
    handler: Arc<H>,
    context: Arc<H::Context>,
    tasks: JoinSet<(RequestId, Outcome)>,
    /// Abort handles by request id. An id is removed when its answer is
    /// reaped or when the client cancels it, so `Cancel` for a finished or
    /// unknown call is a no-op.
    running: HashMap<RequestId, AbortHandle>,
    limit: NonZeroUsize,
}

impl<H> Calls<H>
where
    H: Handler + Send + 'static,
    H::Context: Send + 'static,
{
    pub(super) fn new(handler: Arc<H>, context: Arc<H::Context>, limit: NonZeroUsize) -> Self {
        Self {
            handler,
            context,
            tasks: JoinSet::new(),
            running: HashMap::new(),
            limit,
        }
    }

    fn has_room(&self) -> bool {
        self.running.len() < self.limit.get()
    }

    fn start(&mut self, id: RequestId, call: Call) -> Result<(), ProtocolViolation> {
        let slot = match self.running.entry(id) {
            Entry::Occupied(_) => return Err(ProtocolViolation::DuplicateRequestId(id)),
            Entry::Vacant(slot) => slot,
        };
        tracing::trace!(%id, method = call.method().as_str(), "call");
        let handler = Arc::clone(&self.handler);
        let context = Arc::clone(&self.context);
        let task = self.tasks.spawn(async move {
            let outcome = dispatch(&*handler, &*context, call).await;
            (id, outcome)
        });
        let _ = slot.insert(task);
        Ok(())
    }

    fn cancel(&mut self, id: RequestId) {
        if let Some(task) = self.running.remove(&id) {
            tracing::trace!(%id, "call cancelled by the client");
            task.abort();
        }
    }

    /// The next finished call and its answer; `None` when nothing is in
    /// flight. Cancel-safe: an answer is only taken once it is returned.
    async fn next_finished(&mut self) -> Option<(RequestId, Outcome)> {
        loop {
            match self.tasks.join_next().await? {
                Ok((id, outcome)) => {
                    // A call that finished just as it was cancelled is still
                    // answered; the protocol lets the node answer a cancelled
                    // call, and the client ignores it.
                    drop(self.running.remove(&id));
                    return Some((id, outcome));
                }
                // Cancelled calls were forgotten when the client cancelled them.
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    // The handler panicked. Answer its call so the client does
                    // not wait forever; the rest of the connection carries on.
                    let task = error.id();
                    let id = self
                        .running
                        .iter()
                        .find_map(|(id, handle)| (handle.id() == task).then_some(*id));
                    if let Some(id) = id {
                        drop(self.running.remove(&id));
                        tracing::error!(%id, "a call handler panicked");
                        let error = WireError::new(
                            ErrorCode::Internal,
                            "the node failed while handling this call",
                        );
                        return Some((id, Outcome::Err(error)));
                    }
                }
            }
        }
    }
}

/// Read client frames until the client leaves, the transport fails, the
/// client breaks the protocol, or `closing` is cancelled. Cancels `closing`
/// on the way out so the writer and every subscription stop too.
pub(super) async fn read<S, E, H>(
    mut stream: S,
    mut calls: Calls<H>,
    queue: mpsc::Sender<Outgoing>,
    closing: CancellationToken,
) -> Result<(), ServeError<E>>
where
    S: Stream<Item = Result<String, E>> + Unpin,
    H: Handler + Send + 'static,
    H::Context: Send + 'static,
{
    let _close_on_exit = closing.clone().drop_guard();
    loop {
        tokio::select! {
            biased;
            () = closing.cancelled() => return Ok(()),
            Some((id, outcome)) = calls.next_finished() => {
                if queue.send(Outgoing::Response { id, outcome }).await.is_err() {
                    // The writer is gone, so the connection is over.
                    return Ok(());
                }
            }
            frame = stream.next(), if calls.has_room() => {
                let Some(frame) = frame else {
                    return Ok(());
                };
                let text = frame.map_err(ServeError::Transport)?;
                match decode(&text) {
                    Inbound::Call { id, call } => {
                        calls.start(id, call).map_err(ServeError::Violation)?;
                    }
                    Inbound::Cancel(id) => calls.cancel(id),
                    Inbound::Invalid { id, error } => {
                        let error = WireError::new(
                            ErrorCode::InvalidRequest,
                            format!("invalid request: {error}"),
                        );
                        let answer = Outgoing::Response { id, outcome: Outcome::Err(error) };
                        if queue.send(answer).await.is_err() {
                            return Ok(());
                        }
                    }
                    Inbound::Hello => {
                        return Err(ServeError::Violation(ProtocolViolation::RepeatedHello));
                    }
                    Inbound::Garbage(error) => return Err(ServeError::Decode(error)),
                }
            }
        }
    }
}

/// A decoded client frame, as far as this reader cares.
enum Inbound {
    Call {
        id: RequestId,
        call: Call,
    },
    Cancel(RequestId),
    Hello,
    /// A request whose call did not decode (an unknown method or bad
    /// parameters, for example from a newer client); answerable by id.
    Invalid {
        id: RequestId,
        error: serde_json::Error,
    },
    /// Not a client frame at all; there is no id to answer.
    Garbage(serde_json::Error),
}

/// Just enough of a frame to answer a request whose call did not decode.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RequestHeader {
    Request {
        id: RequestId,
    },
    #[serde(other)]
    Other,
}

fn decode(text: &str) -> Inbound {
    match serde_json::from_str::<ClientFrame>(text) {
        Ok(ClientFrame::Request { id, call }) => Inbound::Call { id, call },
        Ok(ClientFrame::Cancel { id }) => Inbound::Cancel(id),
        Ok(ClientFrame::Hello(_)) => Inbound::Hello,
        Err(error) => match serde_json::from_str::<RequestHeader>(text) {
            Ok(RequestHeader::Request { id }) => Inbound::Invalid { id, error },
            Ok(RequestHeader::Other) | Err(_) => Inbound::Garbage(error),
        },
    }
}
