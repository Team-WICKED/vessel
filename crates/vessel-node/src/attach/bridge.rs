//! The byte pump between `attach`'s stdio and the node's local socket:
//! the token goes first, then bytes flow both ways undecoded, except the
//! node's first frame, which tells a refused token from a working
//! connection.

use std::io;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use vessel_proto::{ErrorCode, ProxyExit, ServerFrame};
use vessel_rpc::MAX_FRAME;
use vessel_store::NodeToken;

use crate::ipc;

/// Stdin is forwarded in chunks of at most this size.
const STDIN_CHUNK: usize = 64 * 1024;
/// Chunks buffered between the stdin thread and the socket.
const STDIN_QUEUE: usize = 16;

/// Present the token, then copy bytes both ways until the node closes.
pub(super) async fn bridge(stream: ipc::LocalClient, token: &NodeToken) -> ProxyExit {
    let (mut from_node, mut to_node) = tokio::io::split(stream);
    if let Err(error) = write_frame(&mut to_node, token.expose().as_bytes()).await {
        tracing::warn!(%error, "the node closed before the token was sent");
        return ProxyExit::NodeUnavailable;
    }
    let mut stdin = match stdin_chunks() {
        Ok(stdin) => stdin,
        Err(error) => {
            tracing::error!(%error, "cannot read stdin");
            return ProxyExit::Closed;
        }
    };
    let upstream = async {
        while let Some(chunk) = stdin.recv().await {
            to_node.write_all(&chunk).await?;
            to_node.flush().await?;
        }
        // The client is done; the node sees end of stream and closes.
        to_node.shutdown().await
    };
    let downstream = forward_to_stdout(&mut from_node);
    tokio::pin!(upstream, downstream);
    let mut upstream_done = false;
    loop {
        tokio::select! {
            exit = &mut downstream => return exit,
            sent = &mut upstream, if !upstream_done => {
                upstream_done = true;
                if let Err(error) = sent {
                    tracing::debug!(%error, "node stopped reading");
                }
            }
        }
    }
}

/// Copy the node's output to stdout. The first frame is decoded to see
/// whether the node refused the token.
async fn forward_to_stdout(from_node: &mut (impl AsyncRead + Unpin)) -> ProxyExit {
    let mut stdout = tokio::io::stdout();
    let first = match read_frame(from_node).await {
        Ok(Some(frame)) => frame,
        Ok(None) => return ProxyExit::Closed,
        Err(error) => {
            tracing::debug!(%error, "node connection failed");
            return ProxyExit::Closed;
        }
    };
    if stdout.write_all(&first.raw).await.is_err() || stdout.flush().await.is_err() {
        return ProxyExit::Closed;
    }
    if first.is_refusal() {
        return ProxyExit::Refused;
    }
    let mut buffer = vec![0_u8; STDIN_CHUNK];
    loop {
        let read = match from_node.read(&mut buffer).await {
            Ok(0) | Err(_) => return ProxyExit::Closed,
            Ok(read) => read,
        };
        let Some(chunk) = buffer.get(..read) else {
            return ProxyExit::Closed;
        };
        if stdout.write_all(chunk).await.is_err() || stdout.flush().await.is_err() {
            return ProxyExit::Closed;
        }
    }
}

/// One frame as read: its bytes (length included) and its body.
struct RawFrame {
    raw: Vec<u8>,
}

impl RawFrame {
    fn body(&self) -> &[u8] {
        self.raw.get(4..).unwrap_or_default()
    }

    /// Whether this is the node's `rejected` (`unauthorized`) answer.
    fn is_refusal(&self) -> bool {
        matches!(
            serde_json::from_slice::<ServerFrame>(self.body()),
            Ok(ServerFrame::Rejected { error }) if error.code == ErrorCode::Unauthorized
        )
    }
}

/// Read one frame; `None` at a clean end of stream before it.
async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<RawFrame>> {
    let mut header = [0_u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = usize::try_from(u32::from_be_bytes(header))
        .ok()
        .filter(|length| *length <= MAX_FRAME.get())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    let mut raw = Vec::with_capacity(length.saturating_add(4));
    raw.extend_from_slice(&header);
    raw.resize(length.saturating_add(4), 0);
    if let Some(body) = raw.get_mut(4..) {
        let _read = reader.read_exact(body).await?;
    }
    Ok(Some(RawFrame { raw }))
}

/// Write one frame (PROTOCOL.md version 3 framing).
async fn write_frame(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    body: &[u8],
) -> io::Result<()> {
    let length = u32::try_from(body.len())
        .map_err(|_too_long| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

/// Stdin as chunks, read on a plain thread (see the module docs). Ends at
/// end of input.
fn stdin_chunks() -> io::Result<mpsc::Receiver<Bytes>> {
    use std::io::Read as _;

    let (sender, chunks) = mpsc::channel(STDIN_QUEUE);
    let _detached = std::thread::Builder::new()
        .name("vesseld-attach-stdin".to_owned())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buffer = vec![0_u8; STDIN_CHUNK];
            loop {
                let read = match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                let Some(chunk) = buffer.get(..read) else {
                    return;
                };
                if sender.blocking_send(Bytes::copy_from_slice(chunk)).is_err() {
                    return;
                }
            }
        })?;
    Ok(chunks)
}
