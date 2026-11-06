//! Terminal attach: keystrokes in, the harness's own screen out.
//!
//! The local terminal goes into raw mode so every key (arrows, Ctrl-C,
//! escape sequences) reaches the harness untouched; its output bytes are
//! written to stdout as they come. `Ctrl-]` detaches, as in telnet and
//! `docker attach`, because the harness never needs that key; it is
//! recognised in every keyboard mode a harness can switch the terminal to
//! (`detach`). `SIGTERM`, `SIGHUP` and `SIGINT` detach too (`signals`).
//! However the attach ends, the terminal is reset and leaves raw mode
//! (`guard`).
//!
//! Keystrokes never hold up the screen: they are queued and sent in order
//! by one sender task (`sender`), while this loop keeps showing output and
//! scanning keys.
//!
//! Window size: the size is sent when attaching, and again on every change
//! on Unix (`SIGWINCH`). On Windows only the initial size is sent; following
//! console resizes would need a console event loop, which would also take
//! the raw keystrokes this module forwards.

mod detach;
mod guard;
mod sender;
mod signals;

use std::io::IsTerminal as _;
use std::time::Duration;

use tokio::sync::mpsc;
#[cfg(unix)]
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::Instant;
use vessel_proto::{SessionEvent, SessionId, TerminalSize};

use super::{Ended, Stream, notice};
use crate::error::CliError;
use crate::io;
use detach::{ESCAPE_WAIT, Scanner};
use guard::RawMode;
use sender::{Item, Pending};
use signals::Stop;

/// How long a detach waits for keys typed before `Ctrl-]` to be sent. They
/// normally go at once; a harness that stopped reading its input must not
/// keep the user from leaving.
const DETACH_FLUSH: Duration = Duration::from_secs(1);

pub(super) async fn attach(mut stream: Stream, session: SessionId) -> Result<Ended, CliError> {
    let raw = RawMode::enable()?;
    if raw.is_some() {
        io::note("[vessel] attached; Ctrl-] detaches\r\n").await?;
    }
    let mut stop = Stop::listen();
    let (batches, mut sending) = sender::spawn(stream.client(), session);
    let mut pending = Pending::default();
    if let Some(size) = window_size() {
        pending.resize(size);
    }
    let mut scanner = Scanner::default();
    let mut escape_deadline = Instant::now();
    let mut keys = io::input_chunks()?;
    let mut size_changes = resizes();
    let mut size_changes_open = true;
    let mut input_open = true;
    let mut last_status = None;
    // `break` detaches; every other way out returns. The detach runs after
    // the loop, once the `select!` no longer borrows the sender.
    loop {
        tokio::select! {
            () = stop.requested() => break,
            sent = &mut sending => {
                // The sender only ends on an error it cannot get past, or
                // when the stream is gone (the loop still holds its queue).
                sent??;
                return Ok(Ended::Session(last_status));
            }
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
                Some(Err(error)) if !error.is_fatal() => notice(&error, "\r\n").await?,
                Some(Err(error)) => return Err(error.into()),
            },
            chunk = keys.recv(), if input_open && pending.has_room() => {
                // End of input (piped stdin ran out; a terminal in raw mode
                // never ends): stop reading, keep showing output until the
                // session ends, as `cmd | ssh host` does.
                let Some(chunk) = chunk else {
                    input_open = false;
                    pending.keys(&scanner.release());
                    continue;
                };
                let typed = scanner.feed(&chunk);
                pending.keys(&typed.keys);
                if typed.detach {
                    break;
                }
                if scanner.is_holding() {
                    escape_deadline = Instant::now()
                        .checked_add(ESCAPE_WAIT)
                        .unwrap_or_else(Instant::now);
                }
            }
            () = tokio::time::sleep_until(escape_deadline), if scanner.is_holding() => {
                pending.keys(&scanner.release());
            }
            permit = batches.reserve(), if !pending.is_empty() => {
                // Fails only if the sender ended; its arm reports why.
                if let Ok(permit) = permit {
                    permit.send(pending.take());
                }
            }
            notice = size_changes.recv(), if size_changes_open => {
                if notice.is_none() {
                    size_changes_open = false;
                } else if let Some(size) = window_size() {
                    pending.resize(size);
                }
            }
        }
    }
    Ok(leave(batches, sending, pending).await)
}

/// Detach: hand the sender what is still queued and give it a moment to
/// send it (keys typed before `Ctrl-]` should arrive), then leave.
async fn leave(
    batches: mpsc::Sender<Vec<Item>>,
    sending: tokio::task::JoinHandle<Result<(), CliError>>,
    mut pending: Pending,
) -> Ended {
    let flush = async move {
        if !pending.is_empty() && batches.send(pending.take()).await.is_err() {
            return;
        }
        // Closing the queue lets the sender finish once it is empty.
        drop(batches);
        if let Ok(Err(error)) = sending.await {
            tracing::debug!(%error, "keys before the detach were not all sent");
        }
    };
    if tokio::time::timeout(DETACH_FLUSH, flush).await.is_err() {
        tracing::debug!("the session did not take the last keys in time");
    }
    Ended::Detached
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
