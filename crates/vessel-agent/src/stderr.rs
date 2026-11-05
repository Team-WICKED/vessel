//! The harness's stderr, line by line, as `HarnessLog` events.
//!
//! Kept because login and configuration problems (an expired token, a
//! missing API key) surface only there.

use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, BufReader};
use tokio::sync::mpsc;
use vessel_proto::AgentEvent;

use crate::output::AgentOutput;

/// Longest line kept; the rest of a longer line is dropped. A harness that
/// writes megabytes without a newline must not fill the node's memory.
const LINE_LIMIT: u64 = 8 * 1024;

/// Forward every stderr line until the stream ends.
pub(crate) async fn pump(stderr: impl AsyncRead + Unpin, outputs: mpsc::Sender<AgentOutput>) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_line(&mut reader, &mut line).await {
            Ok(false) => break,
            Ok(true) => {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\n', '\r']);
                let event = AgentEvent::HarnessLog {
                    line: text.to_owned(),
                };
                if outputs.send(AgentOutput::Event(event)).await.is_err() {
                    break;
                }
            }
            Err(error) => {
                tracing::debug!(%error, "harness stderr ended");
                break;
            }
        }
    }
}

/// Read one line (at most [`LINE_LIMIT`] bytes of it) into `line`; `false`
/// at the end of the stream.
async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    let read = (&mut *reader)
        .take(LINE_LIMIT)
        .read_until(b'\n', line)
        .await?;
    if read == 0 {
        return Ok(false);
    }
    if line.last() != Some(&b'\n') && u64::try_from(read).is_ok_and(|read| read == LINE_LIMIT) {
        // Over-long line: skip the rest of it.
        let mut rest = Vec::new();
        loop {
            rest.clear();
            let skipped = (&mut *reader)
                .take(LINE_LIMIT)
                .read_until(b'\n', &mut rest)
                .await?;
            if skipped == 0 || rest.last() == Some(&b'\n') {
                break;
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn lines(input: &[u8]) -> Vec<String> {
        let (sender, mut receiver) = mpsc::channel(16);
        pump(input, sender).await;
        let mut lines = Vec::new();
        while let Some(AgentOutput::Event(AgentEvent::HarnessLog { line })) = receiver.recv().await
        {
            lines.push(line);
        }
        lines
    }

    #[tokio::test]
    async fn splits_lines_and_trims_line_endings() {
        assert_eq!(lines(b"one\r\ntwo\nthree").await, ["one", "two", "three"]);
    }

    #[tokio::test]
    async fn caps_over_long_lines() {
        let mut input = vec![b'x'; 20_000];
        input.extend_from_slice(b"\nnext\n");
        let lines = lines(&input).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 8 * 1024);
        assert_eq!(lines[1], "next");
    }
}
