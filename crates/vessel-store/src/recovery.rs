//! Startup recovery: closing the sessions a previous run left live.
//!
//! A node that stops (cleanly or not) cannot keep its harness processes, so
//! on the next start every session the log still shows as live is over. It
//! gets a final `Status{Interrupted}` record through the normal append path,
//! so its log ends with a final status like every other session's and the
//! `sessions` row follows from the records as usual. The log itself stays
//! readable, and a structured session can be resumed through its harness
//! session id.

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use vessel_proto::{EventRecord, SessionEvent, SessionInfo, SessionStatus};

use crate::error::StoreError;
use crate::{log, reader};

/// Interrupt every live session; returns them as stored afterwards, newest
/// first.
pub(crate) fn interrupt_live(
    connection: &mut Connection,
    now: DateTime<Utc>,
) -> Result<Vec<SessionInfo>, StoreError> {
    let transaction = connection.transaction()?;
    let live = log::live_sessions(&transaction)?;
    let mut interrupted = Vec::with_capacity(live.len());
    for (session, next) in live {
        let record = EventRecord {
            seq: next,
            at: now,
            event: SessionEvent::Status {
                status: SessionStatus::Interrupted,
            },
        };
        let _appended = log::append(&transaction, session, [&record])?;
        if let Some(info) = reader::select_session(&transaction, session)? {
            interrupted.push(info);
        }
    }
    transaction.commit()?;
    interrupted.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(interrupted)
}
