//! Terminal attach: keystrokes in, the harness's own screen out.
//!
//! The local terminal goes into raw mode so every key (arrows, Ctrl-C,
//! escape sequences) reaches the harness untouched; its output bytes are
//! written to stdout as they come. `Ctrl-]` detaches, as in telnet and
//! `docker attach`, because the harness never needs that key.
//!
//! Window size: the size is sent when attaching, and again on every change
//! on Unix (`SIGWINCH`). On Windows only the initial size is sent; following
//! console resizes would need a console event loop, which would also take
//! the raw keystrokes this module forwards.

use std::io::IsTerminal as _;

use bytes::Bytes;
use tokio::sync::mpsc;
#[cfg(unix)]
use tokio::sync::mpsc::error::TrySendError;
use vessel_client::NodeClient;
use vessel_proto::{
    EventSeq, SessionEvent, SessionId, SessionInput, TerminalBytes, TerminalInput, TerminalSize,
};

use super::{Ended, send};
use crate::error::CliError;
use crate::io;

/// `Ctrl-]`.
const DETACH: u8 = 0x1d;

pub(super) async fn attach(
    client: &NodeClient,
    session: SessionId,
    from: EventSeq,
) -> Result<Ended, CliError> {
    let mut stream = client.follow(session, from).await?;
    let raw = RawMode::enable()?;
    if raw.is_some() {
        io::note("[vessel] attached; Ctrl-] detaches\r\n").await?;
    }
    if let Some(size) = window_size() {
        send(client, session, resize(size)).await?;
    }
    let mut keys = io::input_chunks()?;
    let mut size_changes = resizes();
    let mut size_changes_open = true;
    let mut input_open = true;
    let mut last_status = None;
    loop {
        tokio::select! {
            item = stream.next() => match item {
                None => return Ok(Ended::Session(last_status)),
                Some(Ok(record)) => match record.event {
                    SessionEvent::Terminal(output) => io::out(output.data.bytes()).await?,
                    SessionEvent::Status { status } => {
                        if !status.is_live() {
                            last_status = Some(status);
                        }
                    }
                    SessionEvent::Agent { .. } => {}
                },
                Some(Err(error)) if !error.is_fatal() => {
                    io::note(&format!("\r\n[vessel] {error}\r\n")).await?;
                }
                Some(Err(error)) => return Err(error.into()),
            },
            chunk = keys.recv(), if input_open => {
                // End of input (piped stdin ran out; a terminal in raw mode
                // never ends): stop reading, keep showing output until the
                // session ends, as `cmd | ssh host` does.
                let Some(chunk) = chunk else {
                    input_open = false;
                    continue;
                };
                let (keys, detach) = until_detach(chunk);
                if !keys.is_empty() {
                    let data = TerminalInput::Data { data: TerminalBytes::new(keys) };
                    send(client, session, SessionInput::Terminal { input: data }).await?;
                }
                if detach {
                    return Ok(Ended::Detached);
                }
            }
            notice = size_changes.recv(), if size_changes_open => {
                if notice.is_none() {
                    size_changes_open = false;
                } else if let Some(size) = window_size() {
                    send(client, session, resize(size)).await?;
                }
            }
        }
    }
}

/// The keys before a detach key, and whether there was one.
fn until_detach(chunk: Bytes) -> (Bytes, bool) {
    match chunk.iter().position(|&byte| byte == DETACH) {
        Some(at) => (chunk.slice(..at), true),
        None => (chunk, false),
    }
}

fn resize(size: TerminalSize) -> SessionInput {
    SessionInput::Terminal {
        input: TerminalInput::Resize { size },
    }
}

/// The local terminal's size, if stdout is a terminal.
pub(crate) fn window_size() -> Option<TerminalSize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    crossterm::terminal::size()
        .ok()
        .map(|(cols, rows)| TerminalSize { cols, rows })
}

/// Raw mode while alive; the terminal is restored when it drops, on every
/// path out of `attach` (detach, session end, error).
struct RawMode;

impl RawMode {
    /// `None` when stdin is not a terminal (piped input stays as it is).
    fn enable() -> std::io::Result<Option<Self>> {
        if !std::io::stdin().is_terminal() {
            return Ok(None);
        }
        crossterm::terminal::enable_raw_mode()?;
        Ok(Some(Self))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Err(error) = crossterm::terminal::disable_raw_mode() {
            tracing::warn!(%error, "could not restore the terminal; run `reset`");
        }
    }
}

/// Notices that the local terminal changed size. On Unix a task turns
/// `SIGWINCH` into notices; elsewhere the channel closes at once and the
/// size sent when attaching stays (see the module documentation). Capacity
/// one: a notice already waiting covers any further change.
fn resizes() -> mpsc::Receiver<()> {
    let (sender, receiver) = mpsc::channel(1);
    #[cfg(unix)]
    listen_for_resizes(sender);
    #[cfg(not(unix))]
    drop(sender);
    receiver
}

#[cfg(unix)]
fn listen_for_resizes(sender: mpsc::Sender<()>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut changes = match signal(SignalKind::window_change()) {
        Ok(changes) => changes,
        Err(error) => {
            tracing::warn!(%error, "cannot follow terminal size changes");
            return;
        }
    };
    drop(tokio::spawn(async move {
        while changes.recv().await.is_some() {
            if let Err(TrySendError::Closed(())) = sender.try_send(()) {
                return;
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_detach_key_cuts_the_input() {
        let (keys, detach) = until_detach(Bytes::from_static(b"ls\r\x1dmore"));
        assert_eq!((keys.as_ref(), detach), (&b"ls\r"[..], true));
        let (keys, detach) = until_detach(Bytes::from_static(b"plain"));
        assert_eq!((keys.as_ref(), detach), (&b"plain"[..], false));
    }
}
