//! Reads, run on tokio's blocking pool with their own connections.
//!
//! WAL mode lets these run while the writer commits. Connections are reused
//! through a small idle list instead of opened per read, because opening
//! one also re-reads the schema. The list's lock is held only to push or pop
//! a connection, never during a query.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};
use vessel_proto::{EventPage, EventSeq, SessionId, SessionInfo};

use crate::codec::{SESSION_COLUMNS, record_from_row, seq_from_sql, seq_to_sql, session_from_row};
use crate::config::StoreConfig;
use crate::error::StoreError;
use crate::schema;

/// The read half of a store.
#[derive(Debug)]
pub(crate) struct Reader {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    config: StoreConfig,
}

impl Reader {
    pub(crate) const fn new(path: PathBuf, config: StoreConfig) -> Self {
        Self {
            path,
            idle: Mutex::new(Vec::new()),
            config,
        }
    }

    /// Run `read` on an idle connection (or a new one), then keep the
    /// connection for the next read.
    fn with<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        // A poisoned list only means another read panicked while pushing or
        // popping; the list itself is still a valid list.
        let reused = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let connection = match reused {
            Some(connection) => connection,
            None => self.open()?,
        };
        let result = read(&connection);
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < self.config.idle_readers.get() {
            idle.push(connection);
        }
        result
    }

    fn open(&self) -> Result<Connection, StoreError> {
        let connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        schema::configure(&connection)?;
        Ok(connection)
    }

    /// One session, if stored.
    pub(crate) fn session(&self, id: SessionId) -> Result<Option<SessionInfo>, StoreError> {
        self.with(|connection| select_session(connection, id))
    }

    /// Every stored session, newest first.
    pub(crate) fn sessions(&self) -> Result<Vec<SessionInfo>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare_cached(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions ORDER BY created_at DESC"
            ))?;
            let mut rows = statement.query([])?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next()? {
                sessions.push(session_from_row(row)?);
            }
            Ok(sessions)
        })
    }

    /// A page of a session's log from `from` (or from its oldest retained
    /// record, if older ones were pruned). At most `limit` records, capped by
    /// the configured page limits; at least one record if any exist (a
    /// `limit` of zero is read as one, so a page never comes back empty
    /// while claiming more follow, which would make a reader loop).
    pub(crate) fn read_events(
        &self,
        session: SessionId,
        from: EventSeq,
        limit: u32,
    ) -> Result<EventPage, StoreError> {
        let limit = limit.clamp(1, self.config.max_page_records.get());
        let max_bytes = usize::try_from(self.config.max_page_bytes.get()).unwrap_or(usize::MAX);
        self.with(|connection| {
            // One read transaction, so the bounds and the rows agree even
            // while the writer commits.
            let transaction = connection.unchecked_transaction()?;
            let id = session.to_string();
            let (first, next) = transaction
                .prepare_cached("SELECT first_seq, next_seq FROM sessions WHERE id = ?1")?
                .query_row([&id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })
                .optional()?
                .ok_or(StoreError::UnknownSession(session))?;
            let (first, end) = (seq_from_sql(first)?, seq_from_sql(next)?);
            let start = from.max(first);

            let mut statement = transaction.prepare_cached(
                "SELECT seq, at, event FROM events
                 WHERE session_id = ?1 AND seq >= ?2 ORDER BY seq LIMIT ?3",
            )?;
            let mut rows = statement.query(params![id, seq_to_sql(start)?, limit])?;
            let mut records = Vec::new();
            let mut bytes: usize = 0;
            while bytes < max_bytes {
                let Some(row) = rows.next()? else { break };
                let (record, size) = record_from_row(session, row)?;
                bytes = bytes.saturating_add(size);
                records.push(record);
            }
            let next = records
                .last()
                .and_then(|record| record.seq.next())
                .unwrap_or(start);
            Ok(EventPage {
                records,
                next,
                at_end: next >= end,
            })
        })
    }
}

/// One session, if stored, on any connection (also used by recovery inside
/// its write transaction).
pub(crate) fn select_session(
    connection: &Connection,
    id: SessionId,
) -> Result<Option<SessionInfo>, StoreError> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"
    ))?;
    let mut rows = statement.query([id.to_string()])?;
    rows.next()?.map(session_from_row).transpose()
}
