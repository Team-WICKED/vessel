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

/// Keep the last lines of `pipe` in `tail`, logging each at debug level.
pub(super) async fn collect_stderr(pipe: impl AsyncRead + Unpin, tail: Arc<Mutex<StderrTail>>) {
    let mut lines = tokio::io::BufReader::new(pipe).split(b'\n');
    while let Ok(Some(line)) = lines.next_segment().await {
        let line = String::from_utf8_lossy(&line).trim_end().to_owned();
        tracing::debug!(%line, "ssh stderr");
        if let Ok(mut tail) = tail.lock() {
            tail.push(line);
        }
    }
}
