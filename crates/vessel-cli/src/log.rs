//! `vessel log`: a session's log, printed in record order.
//!
//! [Decision Log]
//! - Purpose: the log must read in sequence order whether stdout and
//!   stderr go to one terminal, are merged (`2>&1`), or are split
//!   (`> session.log`).
//! - Constraint: the session's own output (terminal bytes, the
//!   conversation) goes to stdout and Vessel's status lines to stderr, so
//!   `vessel log ID > file` captures exactly what the harness printed and
//!   `cat file` replays its screen. Two descriptors have no common order
//!   except the order of the writes.
//! - Alternatives: everything on stdout (simple, but status lines would end
//!   up inside the captured terminal bytes, in the middle of a replayed
//!   screen); one write per stream per page, as before (statuses of a page
//!   then came out after all of its output).
//! - Choice: render each page into runs of consecutive records that go to
//!   the same stream, and write the runs in order, each flushed before the
//!   next starts (`io::out` and `io::note` flush). Merged output is then in
//!   record order, and split output is as clean as before.
//! - Cost: one write per change of stream instead of two per page; status
//!   records are few, so this is a handful of extra writes per session.

use vessel_client::NodeClient;
use vessel_proto::{EventRecord, EventSeq, SessionEvent, SessionId};

use crate::error::CliError;
use crate::io;
use crate::view::Conversation;

/// Records asked for per page.
const PAGE: u32 = 500;

/// Which stream a run of text goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// The session's output: stdout.
    Out,
    /// Vessel's notes about the session: stderr.
    Note,
}

/// Consecutive text for one stream.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    stream: Stream,
    text: Vec<u8>,
}

/// Print the log of `session` from `from` to its end.
pub(crate) async fn print(
    client: &NodeClient,
    session: SessionId,
    from: EventSeq,
) -> Result<(), CliError> {
    let mut conversation = Conversation::default();
    let mut next = from;
    loop {
        let page = client.read(session, next, PAGE).await?;
        for run in render(&mut conversation, next, &page.records) {
            match run.stream {
                Stream::Out => io::out(&run.text).await?,
                Stream::Note => io::note_bytes(&run.text).await?,
            }
        }
        next = page.next;
        if page.at_end {
            return Ok(());
        }
    }
}

/// A page as runs in record order. `expected` is the first record asked
/// for; a later first record means the ones before it were pruned.
fn render(
    conversation: &mut Conversation,
    expected: EventSeq,
    records: &[EventRecord],
) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut push = |stream: Stream, text: &[u8]| {
        if text.is_empty() {
            return;
        }
        match runs.last_mut() {
            Some(run) if run.stream == stream => run.text.extend_from_slice(text),
            Some(_) | None => runs.push(Run {
                stream,
                text: text.to_vec(),
            }),
        }
    };
    if let Some(first) = records.first()
        && first.seq > expected
    {
        let pruned = format!("[vessel] records {expected}..{} were pruned\n", first.seq);
        push(Stream::Note, pruned.as_bytes());
    }
    for record in records {
        match &record.event {
            SessionEvent::Terminal(output) => push(Stream::Out, output.data.bytes()),
            SessionEvent::Status { .. } => push(Stream::Note, conversation.show(record).as_bytes()),
            SessionEvent::Agent { .. } => push(Stream::Out, conversation.show(record).as_bytes()),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use chrono::DateTime;
    use vessel_proto::{
        AgentEvent, ContentBlock, HarnessSessionId, SessionStatus, TerminalBytes, TerminalOutput,
    };

    use super::*;

    fn record(seq: u64, event: SessionEvent) -> EventRecord {
        EventRecord {
            seq: EventSeq::new(seq),
            at: DateTime::UNIX_EPOCH,
            event,
        }
    }

    fn status(seq: u64, status: SessionStatus) -> EventRecord {
        record(seq, SessionEvent::Status { status })
    }

    fn agent(seq: u64, event: AgentEvent) -> EventRecord {
        record(seq, SessionEvent::Agent { event })
    }

    fn terminal(seq: u64, text: &'static str) -> EventRecord {
        record(
            seq,
            SessionEvent::Terminal(TerminalOutput {
                data: TerminalBytes::new(Bytes::from_static(text.as_bytes())),
            }),
        )
    }

    fn runs(records: &[EventRecord], expected: u64) -> Vec<(Stream, String)> {
        render(
            &mut Conversation::default(),
            EventSeq::new(expected),
            records,
        )
        .into_iter()
        .map(|run| (run.stream, String::from_utf8(run.text).unwrap()))
        .collect()
    }

    #[test]
    fn statuses_and_output_keep_record_order_across_streams() {
        let records = [
            status(0, SessionStatus::Starting),
            agent(
                1,
                AgentEvent::Ready {
                    harness_session: HarnessSessionId::new("s-1"),
                },
            ),
            status(2, SessionStatus::Running),
            agent(
                3,
                AgentEvent::MessageChunk {
                    content: ContentBlock::Text {
                        text: "hello".to_owned(),
                    },
                },
            ),
            status(4, SessionStatus::Interrupted),
        ];
        assert_eq!(
            runs(&records, 0),
            [
                (Stream::Note, "[session starting]\n".to_owned()),
                (Stream::Out, "[ready: harness session s-1]\n".to_owned()),
                (Stream::Note, "[session running]\n".to_owned()),
                (Stream::Out, "hello".to_owned()),
                (Stream::Note, "\n[session interrupted]\n".to_owned()),
            ]
        );
    }

    #[test]
    fn consecutive_records_of_one_stream_are_one_write() {
        let records = [
            terminal(5, "a"),
            terminal(6, "b"),
            status(7, SessionStatus::Exited { code: Some(0) }),
        ];
        assert_eq!(
            runs(&records, 3),
            [
                (
                    Stream::Note,
                    "[vessel] records 3..5 were pruned\n".to_owned()
                ),
                (Stream::Out, "ab".to_owned()),
                (Stream::Note, "[session exited (0)]\n".to_owned()),
            ]
        );
    }
}
