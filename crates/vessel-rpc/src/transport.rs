//! What a transport must offer: a stream of incoming frames and a sink for
//! outgoing ones, each frame one JSON text.
//!
//! Frames are `String`s rather than a transport's own message type so the
//! same server and client code runs over a websocket, an in-memory pair, and
//! later SSH stdio (newline-delimited) or the relay. Adapters for a specific
//! library live next to the code that owns that library (`vessel-client`
//! for `tokio-tungstenite`, `vessel-node` for axum), so this crate depends on
//! no network stack.

use futures_util::{Sink, Stream};

/// A bidirectional, ordered channel of protocol frames.
///
/// Implemented automatically for anything that is a `Stream` of
/// `Result<String, E>` and a `Sink<String>` with the same error type.
/// End of stream means the peer closed the connection.
pub trait FrameTransport:
    Stream<Item = Result<String, <Self as FrameTransport>::TransportError>>
    + Sink<String, Error = <Self as FrameTransport>::TransportError>
    + Unpin
    + Send
    + 'static
{
    /// Why the transport failed.
    type TransportError: std::error::Error + Send + Sync + 'static;
}

impl<T, E> FrameTransport for T
where
    T: Stream<Item = Result<String, E>> + Sink<String, Error = E> + Unpin + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    type TransportError = E;
}
