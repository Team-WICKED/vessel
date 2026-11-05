//! The CLI's terminal I/O.
//!
//! Output goes through tokio's stdout and stderr handles (one writer each,
//! flushed per write), so printing never blocks a runtime thread.
//!
//! Input is read on a dedicated thread feeding a small bounded channel.
//! `tokio::io::stdin` is not used because a read blocked on an interactive
//! terminal would keep the runtime from shutting down after a detach; a
//! plain thread is simply abandoned when the process exits. When the
//! channel is full the thread waits, so unread input stays in the terminal's
//! own buffer rather than ours.

use std::io::{BufRead as _, Read as _};

use bytes::Bytes;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

/// Chunks of input buffered between the reader thread and the command.
const INPUT_QUEUE: usize = 16;
/// Largest chunk read at once.
const CHUNK: usize = 4096;

/// Write `text` to stdout.
pub(crate) async fn out(bytes: &[u8]) -> std::io::Result<()> {
    let mut stdout = tokio::io::stdout();
    stdout.write_all(bytes).await?;
    stdout.flush().await
}

/// Write `text` to stderr (status lines that are not the command's output).
pub(crate) async fn note(text: &str) -> std::io::Result<()> {
    let mut stderr = tokio::io::stderr();
    stderr.write_all(text.as_bytes()).await?;
    stderr.flush().await
}

/// Raw stdin bytes, as they arrive (keystrokes in raw mode). Ends at EOF.
pub(crate) fn input_chunks() -> std::io::Result<mpsc::Receiver<Bytes>> {
    let (sender, chunks) = mpsc::channel(INPUT_QUEUE);
    let _detached = std::thread::Builder::new()
        .name("vessel-stdin".to_owned())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buffer = vec![0_u8; CHUNK];
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

/// Stdin lines, without their line ending. Ends at EOF.
pub(crate) fn input_lines() -> std::io::Result<mpsc::Receiver<String>> {
    let (sender, lines) = mpsc::channel(INPUT_QUEUE);
    let _detached = std::thread::Builder::new()
        .name("vessel-stdin".to_owned())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { return };
                if sender.blocking_send(line).is_err() {
                    return;
                }
            }
        })?;
    Ok(lines)
}
