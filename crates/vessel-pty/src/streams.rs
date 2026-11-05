//! [`PtyInput`] and [`PtyOutput`]: the async ends of a PTY's byte streams.
//!
//! Only channel ends live here; the blocking I/O behind them is in
//! `threads`.

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::error::PtyClosed;
use crate::size::WindowSize;
use crate::threads::Control;

/// Cloneable input side of a [`PtyProcess`](crate::PtyProcess).
///
/// Holds only a weak reference, so outstanding handles never keep a PTY
/// open after its program has exited.
#[derive(Debug, Clone)]
pub struct PtyInput {
    control: mpsc::WeakSender<Control>,
}

impl PtyInput {
    pub(crate) const fn new(control: mpsc::WeakSender<Control>) -> Self {
        Self { control }
    }

    /// Write bytes to the program's terminal input. Waits while the input
    /// queue is full.
    ///
    /// # Errors
    /// [`PtyClosed`] once the program has exited.
    pub async fn write(&self, data: Bytes) -> Result<(), PtyClosed> {
        self.send(Control::Write(data)).await
    }

    /// Change the window size. Applied in order with writes.
    ///
    /// # Errors
    /// [`PtyClosed`] once the program has exited.
    pub async fn resize(&self, size: WindowSize) -> Result<(), PtyClosed> {
        self.send(Control::Resize(size)).await
    }

    async fn send(&self, message: Control) -> Result<(), PtyClosed> {
        let control = self.control.upgrade().ok_or(PtyClosed)?;
        if control.send(message).await.is_err() {
            return Err(PtyClosed);
        }
        Ok(())
    }
}

/// Output of a [`PtyProcess`](crate::PtyProcess), as raw byte chunks in
/// order.
#[derive(Debug)]
pub struct PtyOutput {
    chunks: mpsc::Receiver<Bytes>,
}

impl PtyOutput {
    pub(crate) const fn new(chunks: mpsc::Receiver<Bytes>) -> Self {
        Self { chunks }
    }

    /// The next chunk, or `None` once the PTY is closed and drained.
    /// Cancel-safe.
    pub async fn next(&mut self) -> Option<Bytes> {
        self.chunks.recv().await
    }
}
