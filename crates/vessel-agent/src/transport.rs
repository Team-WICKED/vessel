//! The agent's stdio as the SDK's line transport, with backpressure.
//!
//! [Decision Log]
//! - Purpose: the SDK reads every line the agent writes into unbounded
//!   queues (its transport channel is unbounded by design; the transport is
//!   expected to push back). With the SDK's own byte transport, a stalled
//!   consumer (a slow store, a full durable channel) stopped the driver but
//!   not the reading: the agent was never blocked, and its output piled up
//!   in memory, against the rule that a slow consumer slows the producer.
//! - Choice: hand the SDK our own line stream (`Lines`) that holds each
//!   line the driver will handle back until a [`Gate`] permit is free. The
//!   driver returns the permit when it takes the item from its queue. When
//!   the driver stalls, the stream stops reading, the OS pipe fills, and
//!   the agent blocks on write.
//! - Which lines count: the agent's `session/update` notifications and
//!   `session/request_permission` requests, each of which becomes exactly
//!   one driver item. The stream parses their parameters into the same ACP
//!   types the SDK does, so a line it counts is one the SDK delivers to
//!   the driver; a malformed one passes uncounted and cannot leak a permit.
//! - What is left unbounded: responses to Vessel's own requests (as many
//!   as Vessel has outstanding: a handful) and methods Vessel does not
//!   handle, which the SDK answers or drops by itself. Those only queue up
//!   while the driver is stalled and the agent misbehaves; recorded in ADR
//!   0003.

use std::borrow::Cow;
use std::io;
use std::sync::Arc;

use agent_client_protocol::Lines;
use agent_client_protocol::v1::schema as acp;
use futures_util::{Sink, Stream};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::value::RawValue;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::Semaphore;

/// Lines from the agent the driver may hold at once (counted lines only).
pub(crate) const IN_FLIGHT: usize = 64;

/// Permits for counted lines: taken by the stream, returned by the driver.
#[derive(Debug, Clone)]
pub(crate) struct Gate(Arc<Semaphore>);

impl Gate {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Semaphore::new(IN_FLIGHT)))
    }

    /// The driver took one counted item from its queue.
    pub(crate) fn release(&self) {
        self.0.add_permits(1);
    }

    async fn take(&self) {
        // Never closed; a closed gate would only mean nothing waits anymore.
        if let Ok(permit) = Arc::clone(&self.0).acquire_owned().await {
            permit.forget();
        }
    }
}

/// The SDK transport over `outgoing` (the agent's stdin) and `incoming` (its
/// stdout), with `gate` holding counted lines back.
pub(crate) fn lines<W, R>(
    outgoing: W,
    incoming: R,
    gate: Gate,
) -> Lines<
    impl Sink<String, Error = io::Error> + Send + 'static,
    impl Stream<Item = io::Result<String>> + Send + 'static,
>
where
    W: AsyncWrite + Send + Unpin + 'static,
    R: AsyncRead + Send + Unpin + 'static,
{
    Lines::new(writer(outgoing), reader(incoming, gate))
}

/// One line per message, flushed, as the SDK's own byte transport writes.
fn writer<W>(outgoing: W) -> impl Sink<String, Error = io::Error> + Send + 'static
where
    W: AsyncWrite + Send + Unpin + 'static,
{
    futures_util::sink::unfold(outgoing, async |mut outgoing, line: String| {
        outgoing.write_all(line.as_bytes()).await?;
        outgoing.write_all(b"\n").await?;
        outgoing.flush().await?;
        Ok(outgoing)
    })
}

/// The agent's lines, each counted one only once a permit is free.
fn reader<R>(incoming: R, gate: Gate) -> impl Stream<Item = io::Result<String>> + Send + 'static
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let lines = BufReader::new(incoming).lines();
    futures_util::stream::unfold((lines, gate), async |(mut lines, gate)| {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return None,
            // Not UTF-8, or the pipe failed: the SDK ends the connection.
            Err(error) => return Some((Err(error), (lines, gate))),
        };
        if counted(&line) {
            gate.take().await;
        }
        Some((Ok(line), (lines, gate)))
    })
}

/// Just enough of a JSON-RPC message to classify it.
#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(borrow)]
    method: Option<Cow<'a, str>>,
    id: Option<IgnoredAny>,
    #[serde(borrow)]
    params: Option<&'a RawValue>,
}

/// Whether the SDK will hand `line` to the driver as one queue item: a
/// `session/update` notification or a `session/request_permission`
/// request whose parameters parse.
fn counted(line: &str) -> bool {
    let Ok(envelope) = serde_json::from_str::<Envelope<'_>>(line) else {
        return false;
    };
    let (Some(method), Some(params)) = (envelope.method, envelope.params) else {
        return false;
    };
    match (method.as_ref(), envelope.id.is_some()) {
        ("session/update", false) => {
            serde_json::from_str::<acp::SessionNotification>(params.get()).is_ok()
        }
        ("session/request_permission", true) => {
            serde_json::from_str::<acp::RequestPermissionRequest>(params.get()).is_ok()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    use super::*;

    const UPDATE: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}"#;
    const RESPONSE: &str = r#"{"jsonrpc":"2.0","id":3,"result":{}}"#;
    const UNKNOWN: &str = r#"{"jsonrpc":"2.0","method":"_vendor/ping","params":{}}"#;
    const MALFORMED: &str =
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":7}}"#;

    #[test]
    fn only_messages_the_driver_handles_are_counted() {
        assert!(counted(UPDATE));
        let permission = r#"{"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c"},"options":[]}}"#;
        assert!(counted(permission));
        assert!(!counted(RESPONSE));
        assert!(!counted(UNKNOWN));
        assert!(!counted(MALFORMED), "the SDK would not deliver it");
        assert!(!counted("not json"));
        assert!(!counted(
            &UPDATE.replace(r#""method""#, r#""id":1,"method""#)
        ));
    }

    /// With every permit taken, a counted line waits until the driver
    /// returns one; other lines keep flowing until then.
    #[tokio::test]
    async fn counted_lines_wait_for_a_permit() {
        let (mut agent, client) = tokio::io::duplex(64 * 1024);
        let gate = Gate::new();
        let mut stream = Box::pin(reader(client, gate.clone()));
        let mut text = String::new();
        for _ in 0..IN_FLIGHT {
            text.push_str(UPDATE);
            text.push('\n');
        }
        for line in [RESPONSE, UNKNOWN, UPDATE] {
            text.push_str(line);
            text.push('\n');
        }
        agent.write_all(text.as_bytes()).await.unwrap();

        for _ in 0..IN_FLIGHT {
            assert_eq!(stream.next().await.unwrap().unwrap(), UPDATE);
        }
        assert_eq!(stream.next().await.unwrap().unwrap(), RESPONSE);
        assert_eq!(stream.next().await.unwrap().unwrap(), UNKNOWN);
        // The 65th update: no permit until the driver hands one back. (A
        // short wait shows it is held; the release then lets it through.)
        assert!(
            tokio::time::timeout(Duration::from_millis(50), stream.next())
                .await
                .is_err(),
            "a counted line passed without a permit"
        );
        gate.release();
        assert_eq!(stream.next().await.unwrap().unwrap(), UPDATE);
        drop(agent);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn lines_that_are_not_utf8_end_the_stream_with_an_error() {
        let (mut agent, client) = tokio::io::duplex(1024);
        let mut stream = Box::pin(reader(client, Gate::new()));
        agent.write_all(b"\xff\xfe\n").await.unwrap();
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
