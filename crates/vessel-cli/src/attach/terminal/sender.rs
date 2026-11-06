//! Keystrokes and resizes on their way to the session: queued by the
//! attach loop, sent in order by one task.
//!
//! [Decision Log]
//! - Purpose: a send waits for the node's answer, and while the harness is
//!   not reading its input (PTY backpressure) that answer waits too.
//!   Awaiting each send inside the attach loop stalled the screen and
//!   stopped the loop from reading keys, so even `Ctrl-]` went unseen.
//! - Choice: one sender task sends everything in order. One task, because
//!   the node runs a connection's calls concurrently: two sends in flight
//!   at once could reach the terminal in either order. The attach loop
//!   keeps a [`Pending`] queue and hands all of it over whenever the sender
//!   is idle (a channel of capacity one), so keys typed during a slow send
//!   go out together as one send. Meanwhile the loop keeps showing output
//!   and scanning keys, so a detach is seen at once.
//! - Bound: [`Pending`] holds at most [`MAX_PENDING`] bytes of keys. Beyond
//!   that the loop stops reading keys until the sender catches up, and
//!   further input waits in the terminal's own buffer; only a large paste
//!   into a harness that stopped reading gets there.
//! - Alternatives: a task per send (reorders keys); an unbounded queue (the
//!   rules forbid it, and a stuck harness would grow it without end).

use std::collections::VecDeque;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use vessel_client::NodeClient;
use vessel_proto::{SessionId, SessionInput, TerminalBytes, TerminalInput, TerminalSize};

use crate::attach::send;
use crate::error::CliError;

/// Most bytes of keys queued while a send is in flight.
const MAX_PENDING: usize = 1024 * 1024;

/// One thing to send.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Item {
    Keys(Vec<u8>),
    Resize(TerminalSize),
}

/// What the attach loop has not handed to the sender yet, in order.
#[derive(Debug, Default)]
pub(super) struct Pending {
    items: VecDeque<Item>,
    /// Bytes of keys in `items`.
    bytes: usize,
}

impl Pending {
    /// Queue keystrokes, joining them to keys queued just before.
    pub(super) fn keys(&mut self, keys: &[u8]) {
        if keys.is_empty() {
            return;
        }
        self.bytes = self.bytes.saturating_add(keys.len());
        if let Some(Item::Keys(queued)) = self.items.back_mut() {
            queued.extend_from_slice(keys);
        } else {
            self.items.push_back(Item::Keys(keys.to_vec()));
        }
    }

    /// Queue a size change. A size queued just before is replaced: only
    /// the latest matters. Keys and sizes keep their relative order.
    pub(super) fn resize(&mut self, size: TerminalSize) {
        if let Some(Item::Resize(queued)) = self.items.back_mut() {
            *queued = size;
        } else {
            self.items.push_back(Item::Resize(size));
        }
    }

    /// Whether more keys may be read.
    pub(super) const fn has_room(&self) -> bool {
        self.bytes < MAX_PENDING
    }

    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Everything queued, for the sender.
    pub(super) fn take(&mut self) -> Vec<Item> {
        self.bytes = 0;
        self.items.drain(..).collect()
    }
}

/// Start the sender; returns where to hand batches, and the task, which
/// ends with an error when the connection is lost.
pub(super) fn spawn(
    client: NodeClient,
    session: SessionId,
) -> (mpsc::Sender<Vec<Item>>, JoinHandle<Result<(), CliError>>) {
    // Capacity one: a batch waits here only while the previous one is being
    // sent; everything typed meanwhile stays in `Pending` and is joined.
    let (batches, mut queue) = mpsc::channel::<Vec<Item>>(1);
    let task = tokio::spawn(async move {
        while let Some(batch) = queue.recv().await {
            for item in batch {
                let input = match item {
                    Item::Keys(keys) => TerminalInput::Data {
                        data: TerminalBytes::new(Bytes::from(keys)),
                    },
                    Item::Resize(size) => TerminalInput::Resize { size },
                };
                // A refusal (the session just ended) is shown by `send`;
                // the stream then ends the attach.
                let _accepted = send(&client, session, SessionInput::Terminal { input }).await?;
            }
        }
        Ok(())
    });
    (batches, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(cols: u16) -> TerminalSize {
        TerminalSize { cols, rows: 24 }
    }

    #[test]
    fn keys_are_joined_and_resizes_keep_their_place() {
        let mut pending = Pending::default();
        assert!(pending.is_empty());
        pending.keys(b"l");
        pending.keys(b"s");
        pending.resize(size(80));
        pending.resize(size(100));
        pending.keys(b"\r");
        pending.keys(b"");
        assert_eq!(
            pending.take(),
            [
                Item::Keys(b"ls".to_vec()),
                Item::Resize(size(100)),
                Item::Keys(b"\r".to_vec()),
            ]
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn reading_keys_pauses_at_the_bound_until_the_sender_takes_them() {
        let mut pending = Pending::default();
        let chunk = vec![b'x'; 4096];
        while pending.has_room() {
            pending.keys(&chunk);
        }
        assert!(pending.bytes >= MAX_PENDING);
        assert_eq!(pending.take().len(), 1);
        assert!(pending.has_room());
    }
}
