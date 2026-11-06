//! Server side: runs one client connection from handshake to close.
//!
//! [`Server::serve`] performs the handshake, then runs two futures side by
//! side in the caller's task until the connection ends:
//!
//! - the reader (`calls`) decodes client frames and runs each call as its
//!   own task, at most [`ServerConfig::max_in_flight`] at a time (more are
//!   refused with `Unavailable`, never left unread);
//! - the writer (`writer`) owns the transport's sink and the connection's
//!   subscription table, and writes every frame from one bounded queue.
//!
//! The node's [`Handler`] sees none of this. It gets a per-connection
//! context built from a [`Peer`], whose [`Outbound`] lets it open
//! subscriptions and push their events.

mod calls;
mod encode;
mod outbound;
mod writer;

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use vessel_proto::{
    ClientFrame, ClientInfo, ErrorCode, Handler, Hello, NodeInfo, ProtocolRange, ProtocolVersion,
    ServerFrame, Welcome, WireError,
};

pub use outbound::{Outbound, SubscriptionClosed, SubscriptionSink};

use crate::error::{ProtocolViolation, ServeError};
use crate::transport::FrameTransport;

/// `n` as a `NonZeroUsize` for constants (`NonZeroUsize::new` is not usable
/// with `?` or `unwrap` in a `const`).
const fn at_least_one(n: usize) -> NonZeroUsize {
    match NonZeroUsize::new(n) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    }
}

/// Limits for each server connection.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Calls one connection may run at once. Requests beyond it are answered
    /// `Unavailable` immediately; the connection keeps being read, so
    /// `Cancel` and other calls still get through.
    pub max_in_flight: NonZeroUsize,
    /// Frames queued for the writer. When full, producers wait: call answers
    /// are delayed, and subscription forwarders stop pulling records, so the
    /// node's live stream reports them as lagged instead of buffering.
    pub outbound_capacity: NonZeroUsize,
    /// How long a new connection may take to send `Hello`; a client that
    /// connects and says nothing is dropped instead of holding a slot.
    pub handshake_timeout: Duration,
    /// How long a closing connection may spend on what it still owes the
    /// client: answers to calls that were running when the server began
    /// shutting down (`node.stop{now}` is one), and frames already queued.
    /// A call still running, or a client that stopped reading, is dropped
    /// after this.
    pub close_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_in_flight: at_least_one(64),
            outbound_capacity: at_least_one(256),
            handshake_timeout: Duration::from_secs(10),
            close_timeout: Duration::from_secs(2),
        }
    }
}

/// What the server learned about a client in the handshake, handed to the
/// context factory of [`Server::serve`].
#[derive(Debug)]
pub struct Peer {
    /// The client's self-description from `Hello`.
    pub client: ClientInfo,
    /// The negotiated protocol version.
    pub protocol: ProtocolVersion,
    /// This connection's subscription outlet.
    pub outbound: Outbound,
}

/// Serves protocol connections with one [`Handler`].
pub struct Server<H> {
    handler: Arc<H>,
    /// The node's current description; read for every `Welcome`, so a
    /// change (a node starting to drain) reaches new connections.
    node: watch::Receiver<NodeInfo>,
    config: ServerConfig,
    shutdown: CancellationToken,
}

impl<H> fmt::Debug for Server<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("node", &self.node.borrow().id)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<H> Server<H>
where
    H: Handler + Send + 'static,
    H::Context: Send + 'static,
{
    /// A server for `handler`. The current value of `node` is sent in every
    /// `Welcome` (it stays readable after its sender is gone); cancelling
    /// `shutdown` closes every connection.
    pub const fn new(
        handler: Arc<H>,
        node: watch::Receiver<NodeInfo>,
        config: ServerConfig,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            handler,
            node,
            config,
            shutdown,
        }
    }

    /// Run one connection until the client leaves, the transport fails, or
    /// the server shuts down. `context` builds the handler's per-connection
    /// context once the handshake succeeded.
    ///
    /// Returns `Ok` for an orderly close (by either side). When the server
    /// shuts down, calls already running get up to `close_timeout` to finish
    /// and be answered; otherwise in-flight calls are aborted when the
    /// connection ends.
    ///
    /// # Errors
    /// The transport failed, the client broke the protocol or spoke no
    /// common version, or a frame could not be encoded.
    pub async fn serve<T, F>(
        &self,
        transport: T,
        context: F,
    ) -> Result<(), ServeError<T::TransportError>>
    where
        T: FrameTransport,
        F: FnOnce(Peer) -> H::Context,
    {
        let (mut sink, mut stream) = transport.split();

        let handshake = tokio::time::timeout(self.config.handshake_timeout, async {
            receive_hello(&mut stream, &mut sink).await
        });
        let hello = tokio::select! {
            biased;
            () = self.shutdown.cancelled() => return Ok(()),
            hello = handshake => match hello {
                Ok(Ok(Some(hello))) => hello,
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(error)) => return Err(error),
                Err(_elapsed) => return Err(ServeError::HandshakeTimeout),
            },
        };

        let Some(protocol) = ProtocolRange::CURRENT.negotiate(hello.protocol) else {
            let error = WireError::new(
                ErrorCode::ProtocolMismatch,
                format!(
                    "this node speaks protocol {}..={}, the client offered {}..={}",
                    ProtocolRange::CURRENT.min,
                    ProtocolRange::CURRENT.max,
                    hello.protocol.min,
                    hello.protocol.max
                ),
            );
            reject(&mut sink, error.clone()).await?;
            return Err(ServeError::Rejected(error));
        };
        let node = self.node.borrow().clone();
        let welcome = ServerFrame::Welcome(Welcome { protocol, node });
        send_now(&mut sink, &welcome).await?;
        tracing::debug!(
            client = %hello.client.name,
            client_version = %hello.client.version,
            %protocol,
            "client connected"
        );

        let closing = self.shutdown.child_token();
        let (queue, queue_rx) = mpsc::channel(self.config.outbound_capacity.get());
        let peer = Peer {
            client: hello.client,
            protocol,
            outbound: Outbound::new(queue.clone(), closing.clone()),
        };
        let calls = calls::Calls::new(
            Arc::clone(&self.handler),
            Arc::new(context(peer)),
            self.config.max_in_flight,
        );
        let settled = CancellationToken::new();
        let reader = calls::read(
            stream,
            calls,
            queue,
            closing.clone(),
            settled.clone(),
            self.config.close_timeout,
        );
        let writer = writer::write(sink, queue_rx, closing, settled, self.config.close_timeout);
        let (read, write) = tokio::join!(reader, writer);
        tracing::debug!("client disconnected");
        read.and(write)
    }
}

/// Wait for the client's `Hello`. `None` means the client closed the
/// connection before saying anything.
async fn receive_hello<S, K, E>(
    stream: &mut S,
    sink: &mut K,
) -> Result<Option<Hello>, ServeError<E>>
where
    S: Stream<Item = Result<String, E>> + Unpin,
    K: Sink<String, Error = E> + Unpin,
{
    let Some(frame) = stream.next().await else {
        return Ok(None);
    };
    let text = frame.map_err(ServeError::Transport)?;
    match serde_json::from_str::<ClientFrame>(&text) {
        Ok(ClientFrame::Hello(hello)) => Ok(Some(hello)),
        Ok(ClientFrame::Request { .. } | ClientFrame::Cancel { .. }) => {
            let violation = ProtocolViolation::ExpectedHello;
            reject(
                sink,
                WireError::new(ErrorCode::InvalidRequest, violation.to_string()),
            )
            .await?;
            Err(ServeError::Violation(violation))
        }
        Err(error) => {
            reject(
                sink,
                WireError::new(ErrorCode::InvalidRequest, format!("invalid hello: {error}")),
            )
            .await?;
            Err(ServeError::Decode(error))
        }
    }
}

/// Refuse the handshake and close the sink.
async fn reject<K, E>(sink: &mut K, error: WireError) -> Result<(), ServeError<E>>
where
    K: Sink<String, Error = E> + Unpin,
{
    send_now(sink, &ServerFrame::Rejected { error }).await?;
    sink.close().await.map_err(ServeError::Transport)
}

/// Encode and write one frame, flushing it.
async fn send_now<K, E>(sink: &mut K, frame: &ServerFrame) -> Result<(), ServeError<E>>
where
    K: Sink<String, Error = E> + Unpin,
{
    let text = serde_json::to_string(frame).map_err(ServeError::Encode)?;
    sink.send(text).await.map_err(ServeError::Transport)
}
