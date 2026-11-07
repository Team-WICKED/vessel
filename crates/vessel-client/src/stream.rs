//! [`SessionStream`]: a session's log as one gap-free sequence.
//!
//! The stream is live (a subscription) until the subscription reports
//! `Lagged`, which means this side could not keep up. It then pages the
//! records it missed with `session.read` at its own pace, and subscribes
//! again from its cursor once a page reports the end of the log. The cursor
//! (the next sequence number to yield) is the only state that matters, so
//! nothing is skipped and a record that arrives twice is dropped.

use std::collections::VecDeque;

use vessel_proto::{
    EventRecord, EventSeq, ReadEvents, SessionId, SessionInfo, Subscribe, SubscriptionEnd,
};
use vessel_rpc::{CallError, Connection, Subscription, SubscriptionItem};

use crate::error::StreamError;

/// Records asked for per `session.read` while catching up. The node caps it
/// further by its own page limits.
const CATCH_UP_PAGE: u32 = 500;

/// Where the stream gets its next record.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one per stream, replaced in place, never queued; the live \
              subscription is the common state, so boxing it would cost every stream"
)]
enum Source {
    /// A live subscription.
    Live(Subscription),
    /// Catching up with paged reads; `at_end` once the last page reached the
    /// end of the log.
    Paging {
        buffered: VecDeque<EventRecord>,
        at_end: bool,
    },
    /// The session ended, or a fatal error was reported.
    Done,
}

/// What to do after looking at the source.
enum Step {
    Record(EventRecord),
    CatchUp,
    ReadPage,
    Resubscribe,
    Finish,
    Fail(StreamError),
}

/// A session's records in order, from where [`NodeClient::follow`] started,
/// until the session ends.
///
/// [`NodeClient::follow`]: crate::NodeClient::follow
#[derive(Debug)]
pub struct SessionStream {
    connection: Connection,
    session: SessionId,
    info: SessionInfo,
    /// The next sequence number to yield.
    cursor: EventSeq,
    source: Source,
    /// A record that arrived after a pruned range; yielded right after the
    /// `Missing` error that announced the range.
    held: Option<EventRecord>,
}

impl SessionStream {
    pub(crate) fn new(connection: Connection, from: EventSeq, subscription: Subscription) -> Self {
        Self {
            connection,
            session: subscription.info().id,
            info: subscription.info().clone(),
            cursor: from,
            source: Source::Live(subscription),
            held: None,
        }
    }

    /// The session when the stream opened.
    #[must_use]
    pub const fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// The sequence number of the next record this stream will yield: where
    /// to resume with a new stream later.
    #[must_use]
    pub const fn cursor(&self) -> EventSeq {
        self.cursor
    }

    /// The next record. `None` once the session has ended (after its final
    /// status record) or after a fatal error. A
    /// [`StreamError::Missing`] is not fatal; keep calling.
    pub async fn next(&mut self) -> Option<Result<EventRecord, StreamError>> {
        if let Some(record) = self.held.take() {
            return self.accept(record);
        }
        loop {
            let step = match &mut self.source {
                Source::Done => return None,
                Source::Live(subscription) => match subscription.next().await {
                    SubscriptionItem::Record(record) => Step::Record(record),
                    SubscriptionItem::Ended(SubscriptionEnd::SessionEnded) => Step::Finish,
                    SubscriptionItem::Ended(SubscriptionEnd::Lagged { resume_from }) => {
                        tracing::debug!(session = %self.session, %resume_from, "fell behind; catching up");
                        Step::CatchUp
                    }
                    SubscriptionItem::Ended(SubscriptionEnd::Unsubscribed) => {
                        Step::Fail(StreamError::Unsubscribed)
                    }
                    SubscriptionItem::Disconnected => {
                        Step::Fail(StreamError::Call(CallError::Disconnected))
                    }
                },
                Source::Paging { buffered, at_end } => match buffered.pop_front() {
                    Some(record) => Step::Record(record),
                    None if *at_end => Step::Resubscribe,
                    None => Step::ReadPage,
                },
            };
            match step {
                Step::Record(record) => {
                    if let Some(item) = self.accept(record) {
                        return Some(item);
                    }
                }
                Step::CatchUp => {
                    self.source = Source::Paging {
                        buffered: VecDeque::new(),
                        at_end: false,
                    };
                }
                Step::ReadPage => {
                    if let Err(error) = self.read_page().await {
                        return Some(Err(self.fail(error)));
                    }
                }
                Step::Resubscribe => {
                    let request = Subscribe {
                        session: self.session,
                        from: self.cursor,
                    };
                    match self.connection.subscribe(request).await {
                        Ok(subscription) => self.source = Source::Live(subscription),
                        Err(error) => return Some(Err(self.fail(error.into()))),
                    }
                }
                Step::Finish => {
                    self.source = Source::Done;
                    return None;
                }
                Step::Fail(error) => return Some(Err(self.fail(error))),
            }
        }
    }

    async fn read_page(&mut self) -> Result<(), StreamError> {
        let page = self
            .connection
            .call(ReadEvents {
                session: self.session,
                from: self.cursor,
                limit: CATCH_UP_PAGE,
                wait: None,
            })
            .await?;
        // An empty page that claims more is coming would loop forever;
        // subscribing is the right move either way.
        let at_end = page.at_end || page.records.is_empty();
        self.source = Source::Paging {
            buffered: page.records.into(),
            at_end,
        };
        Ok(())
    }

    /// Yield `record` if it is the next one; drop it if it was already
    /// yielded; report the gap if records before it are gone.
    fn accept(&mut self, record: EventRecord) -> Option<Result<EventRecord, StreamError>> {
        if record.seq < self.cursor {
            return None;
        }
        if record.seq > self.cursor {
            let missing = StreamError::Missing {
                expected: self.cursor,
                resumed_at: record.seq,
            };
            self.cursor = record.seq;
            self.held = Some(record);
            return Some(Err(missing));
        }
        match record.seq.next() {
            Some(next) => self.cursor = next,
            // The log is full; nothing can follow this record.
            None => self.source = Source::Done,
        }
        Some(Ok(record))
    }

    /// End the stream after reporting `error`.
    fn fail(&mut self, error: StreamError) -> StreamError {
        self.source = Source::Done;
        error
    }
}
