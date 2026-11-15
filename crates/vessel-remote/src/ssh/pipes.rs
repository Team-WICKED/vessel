//! The pipes of a running `ssh`: feeding stdin, reading stdout within a
//! bound, keeping the end of stderr, and the handle a long-running
//! command (`attach`) is used through.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Child;

use super::{Input, STDOUT_LIMIT, SshError, StderrTail};

/// A long-running `ssh` command.
#[derive(Debug)]
pub struct Running {
    /// The `ssh` process; its stdin and stdout are piped, stderr is taken.
    pub child: Child,
    pub(super) stderr: Arc<Mutex<StderrTail>>,
}

impl Running {
    /// The end of stderr so far.
    #[must_use]
    pub fn stderr(&self) -> StderrTail {
        self.stderr
            .lock()
            .map(|tail| tail.clone())
            .unwrap_or_default()
    }

    /// A handle to the stderr tail that outlives this value.
    #[must_use]
    pub fn stderr_handle(&self) -> StderrHandle {
        StderrHandle(Arc::clone(&self.stderr))
    }
}

/// Shared access to a running command's [`StderrTail`].
#[derive(Debug, Clone)]
pub struct StderrHandle(Arc<Mutex<StderrTail>>);

impl StderrHandle {
    /// The end of stderr so far.
    #[must_use]
    pub fn snapshot(&self) -> StderrTail {
        self.0.lock().map(|tail| tail.clone()).unwrap_or_default()
    }
}

/// Write `input` to the command and close its stdin. A command that exits
/// without reading everything closes the pipe; that is its exit code's
/// business, not an error here.
pub(super) async fn feed(
    mut stdin: tokio::process::ChildStdin,
    input: Input<'_>,
) -> Result<(), SshError> {
    let written = match input {
        Input::Empty => Ok(()),
        Input::Bytes(bytes) => stdin.write_all(bytes).await,
        Input::File(path) => {
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(|source| SshError::Input {
                    path: path.to_owned(),
                    source,
                })?;
            tokio::io::copy(&mut file, &mut stdin).await.map(drop)
        }
    };
    let flushed = match written {
        Ok(()) => stdin.shutdown().await,
        Err(error) => Err(error),
    };
    match flushed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            tracing::debug!("the remote command closed its stdin early");
            Ok(())
        }
        Err(error) => Err(SshError::Io(error)),
    }
}

/// Read to the end, keeping at most [`STDOUT_LIMIT`] bytes.
pub(super) async fn read_limited(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(kept);
        }
        let room = STDOUT_LIMIT.saturating_sub(kept.len()).min(read);
        if let Some(chunk) = buffer.get(..room) {
            kept.extend_from_slice(chunk);
        }
    }
}

/// The longest stderr line kept, in bytes; the rest of a longer one is
/// dropped (and the kept part marked), so a remote that writes without
/// newlines cannot grow this process's memory.
const STDERR_LINE: usize = 4096;

/// Keep the last lines of `pipe` in `tail`, logging each at debug level.
pub(super) async fn collect_stderr(pipe: impl AsyncRead + Unpin, tail: Arc<Mutex<StderrTail>>) {
    let mut reader = tokio::io::BufReader::new(pipe);
    let mut line = Vec::new();
    let mut cut = false;
    loop {
        let chunk = match reader.fill_buf().await {
            Ok(chunk) if !chunk.is_empty() => chunk,
            Ok(_) | Err(_) => break,
        };
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let content = chunk
            .get(..newline.unwrap_or(chunk.len()))
            .unwrap_or_default();
        let room = STDERR_LINE.saturating_sub(line.len());
        line.extend_from_slice(content.get(..room).unwrap_or(content));
        cut |= content.len() > room;
        let used = newline.map_or(chunk.len(), |at| at.saturating_add(1));
        reader.consume(used);
        if newline.is_some() {
            keep_line(&tail, &mut line, &mut cut);
        }
    }
    if !line.is_empty() || cut {
        keep_line(&tail, &mut line, &mut cut);
    }
}

/// Move one finished line into `tail`.
fn keep_line(tail: &Mutex<StderrTail>, line: &mut Vec<u8>, cut: &mut bool) {
    let mut text = String::from_utf8_lossy(line).trim_end().to_owned();
    if *cut {
        text.push_str(" [cut]");
    }
    tracing::debug!(line = %text, "ssh stderr");
    if let Ok(mut tail) = tail.lock() {
        tail.push(text);
    }
    line.clear();
    *cut = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stderr_lines_are_kept_within_a_bound() {
        let mut input = b"first\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', STDERR_LINE * 3));
        input.extend_from_slice(b"\nlast");
        let tail = Arc::new(Mutex::new(StderrTail::default()));
        collect_stderr(input.as_slice(), Arc::clone(&tail)).await;
        let tail = tail.lock().unwrap().clone();
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "first");
        assert!(lines[1].starts_with('x') && lines[1].ends_with(" [cut]"));
        assert_eq!(lines[1].len(), STDERR_LINE + " [cut]".len());
        assert_eq!(lines[2], "last");
    }
}
