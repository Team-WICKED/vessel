//! One live subscription on the client side.

use tokio::sync::{mpsc, oneshot};
use vessel_proto::{EventRecord, SessionInfo, SubscriptionEnd, SubscriptionId};

use super::driver::{Command, Opened};

/// What [`Subscription::next`] yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionItem {
    /// The next record, in sequence order.
    Record(EventRecord),
    /// The subscription ended; no more records follow. On
    /// `Lagged{resume_from}` nothing was skipped: read or resubscribe from
    /// `resume_from`.
    Ended(SubscriptionEnd),
    /// The connection closed; records after the last one yielded may exist
    /// on the node.
    Disconnected,
}

/// How a subscription finished, remembered so `next` keeps answering.
#[derive(Debug, Clone, Copy)]
enum Finished {
    Ended(SubscriptionEnd),
    Disconnected,
}

/// A session event stream opened by [`Connection::subscribe`]. Dropping it
/// unsubscribes on the node.
///
/// [`Connection::subscribe`]: super::Connection::subscribe
#[derive(Debug)]
pub struct Subscription {
    id: SubscriptionId,
    info: SessionInfo,
    records: mpsc::Receiver<EventRecord>,
    end: oneshot::Receiver<SubscriptionEnd>,
    finished: Option<Finished>,
    commands: mpsc::Sender<Command>,
}

impl Subscription {
    pub(super) fn new(opened: Opened, commands: mpsc::Sender<Command>) -> Self {
        Self {
            id: opened.subscribed.subscription,
            info: opened.subscribed.info,
            records: opened.records,
            end: opened.end,
            finished: None,
            commands,
        }
    }

    /// The node's id for this subscription.
    #[must_use]
    pub const fn id(&self) -> SubscriptionId {
        self.id
    }

    /// The session as it was when the subscription opened.
    #[must_use]
    pub const fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// The next record, or how the subscription ended. After the end, keeps
    /// returning the same end. Cancel-safe.
    pub async fn next(&mut self) -> SubscriptionItem {
        if let Some(finished) = self.finished {
            return finished.into();
        }
        if let Some(record) = self.records.recv().await {
            return SubscriptionItem::Record(record);
        }
        // The driver closes the record channel right after sending the end
        // reason, so the reason is available once the records are drained.
        let finished = match (&mut self.end).await {
            Ok(end) => Finished::Ended(end),
            Err(_driver_gone) => Finished::Disconnected,
        };
        self.finished = Some(finished);
        finished.into()
    }
}

impl From<Finished> for SubscriptionItem {
    fn from(finished: Finished) -> Self {
        match finished {
            Finished::Ended(end) => Self::Ended(end),
            Finished::Disconnected => Self::Disconnected,
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if self.finished.is_some() {
            return;
        }
        // Best effort: if the queue is full, the driver notices the closed
        // record channel on the next record and unsubscribes then.
        if self
            .commands
            .try_send(Command::Unsubscribe(self.id))
            .is_err()
        {
            tracing::trace!(subscription = %self.id, "could not queue unsubscribe");
        }
    }
}
