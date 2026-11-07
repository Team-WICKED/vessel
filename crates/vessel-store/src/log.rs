//! Write-side operations on the log, run inside the writer's transaction
//! (or recovery's). They take a plain `Connection` so a caller can wrap
//! each one in its own savepoint.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension as _, params};
use vessel_proto::{
    AgentEvent, EventRecord, EventSeq, HarnessSessionId, ProfileName, SessionEvent, SessionId,
    SessionInfo, SessionStatus,
};

use crate::codec::{
    bytes_from_sql, bytes_to_sql, encode, seq_from_sql, seq_to_sql, session_id_from_sql,
    time_to_sql,
};
use crate::config::{ByteCount, Retention};
use crate::error::StoreError;

/// Store a new session with an empty log.
///
/// Only the session's identity, spec, profile and creator come from
/// `info`. Its status is
/// stored as `Starting` and live, and everything else is derived from the
/// records appended later: `info` is a snapshot the runtime took after the
/// process started, and a session that ends within milliseconds would
/// otherwise be stored as ended with no end time and an empty log (and be
/// the first one retention prunes, under the appends still coming).
pub(crate) fn create_session(
    connection: &Connection,
    info: &SessionInfo,
) -> Result<(), StoreError> {
    let inserted = connection
        .prepare_cached(
            "INSERT INTO sessions
                 (id, spec, status, live, created_at, ended_at, next_seq, first_seq,
                  retained_bytes, harness_session, profile, created_by, via, origin)
             VALUES (?1, ?2, ?3, 1, ?4, NULL, 0, 0, 0, NULL, ?5, ?6, ?7, ?8)
             ON CONFLICT (id) DO NOTHING",
        )?
        .execute(params![
            info.id.to_string(),
            encode(&info.spec)?,
            encode(&SessionStatus::Starting)?,
            time_to_sql(info.created_at)?,
            info.profile.as_ref().map(ProfileName::as_str),
            encode(&info.created_by)?,
            info.via.map(|node| node.to_string()),
            info.origin.as_ref().map(encode).transpose()?,
        ])?;
    if inserted == 0 {
        return Err(StoreError::SessionExists(info.id));
    }
    Ok(())
}

/// What appending changed, for the caller's retention pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Appended {
    /// The session reached a final status in this append.
    pub(crate) ended: bool,
}

/// Whether a run of records moved the session to another profile.
#[derive(Default)]
enum ProfileMove {
    /// No `ProfileChanged` in the run.
    #[default]
    Unchanged,
    /// Moved; `None` is the harness's default profile.
    To(Option<ProfileName>),
}

/// The `sessions` columns a run of records changes.
#[derive(Default)]
struct Projection {
    status: Option<SessionStatus>,
    ended_at: Option<DateTime<Utc>>,
    harness_session: Option<HarnessSessionId>,
    /// The last profile change, if any.
    profile: ProfileMove,
    bytes: u64,
}

impl Projection {
    fn observe(&mut self, record: &EventRecord, stored_bytes: usize) {
        let stored_bytes = u64::try_from(stored_bytes).unwrap_or(u64::MAX);
        self.bytes = self.bytes.saturating_add(stored_bytes);
        match &record.event {
            SessionEvent::Status { status } => {
                if !status.is_live() {
                    self.ended_at = Some(record.at);
                }
                self.status = Some(status.clone());
            }
            SessionEvent::Agent {
                event: AgentEvent::Ready { harness_session },
            } => self.harness_session = Some(harness_session.clone()),
            SessionEvent::ProfileChanged { to, .. } => {
                self.profile = ProfileMove::To(to.clone());
            }
            SessionEvent::Agent { .. } | SessionEvent::Terminal(_) => {}
        }
    }
}

/// Append records to a session's log. They must continue it exactly:
/// the first has the session's next sequence number and there are no gaps.
pub(crate) fn append<'a>(
    connection: &Connection,
    session: SessionId,
    records: impl IntoIterator<Item = &'a EventRecord>,
) -> Result<Appended, StoreError> {
    let id = session.to_string();
    let next: i64 = connection
        .prepare_cached("SELECT next_seq FROM sessions WHERE id = ?1")?
        .query_row([&id], |row| row.get(0))
        .optional()?
        .ok_or(StoreError::UnknownSession(session))?;
    let mut expected = seq_from_sql(next)?;
    let mut projection = Projection::default();
    let mut insert = connection.prepare_cached(
        "INSERT INTO events (session_id, seq, at, event) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for record in records {
        if record.seq != expected {
            return Err(StoreError::OutOfOrder {
                session,
                expected,
                got: record.seq,
            });
        }
        let event = encode(&record.event)?;
        let _rows = insert.execute(params![
            id,
            seq_to_sql(record.seq)?,
            time_to_sql(record.at)?,
            event
        ])?;
        projection.observe(record, event.len());
        // Records past u64::MAX cannot exist: the runtime ends a session
        // before its counter would wrap.
        expected = expected
            .next()
            .ok_or(StoreError::OutOfRange(crate::error::Quantity::Seq))?;
    }
    let ended = projection
        .status
        .as_ref()
        .is_some_and(|status| !status.is_live());
    let _rows = connection
        .prepare_cached(
            "UPDATE sessions SET
                 next_seq = ?2,
                 retained_bytes = retained_bytes + ?3,
                 status = COALESCE(?4, status),
                 live = COALESCE(?5, live),
                 ended_at = COALESCE(?6, ended_at),
                 harness_session = COALESCE(?7, harness_session),
                 profile = CASE WHEN ?8 THEN ?9 ELSE profile END
             WHERE id = ?1",
        )?
        .execute(params![
            id,
            seq_to_sql(expected)?,
            bytes_to_sql(ByteCount::new(projection.bytes))?,
            projection.status.as_ref().map(encode).transpose()?,
            projection.status.as_ref().map(SessionStatus::is_live),
            projection.ended_at.map(time_to_sql).transpose()?,
            projection
                .harness_session
                .as_ref()
                .map(HarnessSessionId::as_str),
            matches!(projection.profile, ProfileMove::To(_)),
            match &projection.profile {
                ProfileMove::To(Some(name)) => Some(name.as_str()),
                ProfileMove::To(None) | ProfileMove::Unchanged => None,
            },
        ])?;
    Ok(Appended { ended })
}

/// Delete a session's oldest records until it is within the byte cap.
/// The newest record always stays, so a session's final status is never
/// lost to retention.
pub(crate) fn prune_session(
    connection: &Connection,
    session: SessionId,
    retention: &Retention,
) -> Result<(), StoreError> {
    let id = session.to_string();
    let Some((retained, next)) = connection
        .prepare_cached("SELECT retained_bytes, next_seq FROM sessions WHERE id = ?1")?
        .query_row([&id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .optional()?
    else {
        return Ok(());
    };
    let retained = bytes_from_sql(retained)?.get();
    let Some(excess) = retained.checked_sub(retention.max_session_bytes.get()) else {
        return Ok(());
    };
    if excess == 0 {
        return Ok(());
    }
    let newest = next.saturating_sub(1);
    let mut oldest_first = connection.prepare_cached(
        "SELECT seq, length(CAST(event AS BLOB)) FROM events
         WHERE session_id = ?1 AND seq < ?2 ORDER BY seq",
    )?;
    let mut rows = oldest_first.query(params![id, newest])?;
    let mut freed: u64 = 0;
    let mut cut: Option<i64> = None;
    while freed < excess {
        let Some(row) = rows.next()? else { break };
        cut = Some(row.get(0)?);
        freed = freed.saturating_add(bytes_from_sql(row.get(1)?)?.get());
    }
    drop(rows);
    let Some(cut) = cut else { return Ok(()) };
    let _rows = connection
        .prepare_cached("DELETE FROM events WHERE session_id = ?1 AND seq <= ?2")?
        .execute(params![id, cut])?;
    let _rows = connection
        .prepare_cached(
            "UPDATE sessions SET first_seq = ?2, retained_bytes = retained_bytes - ?3
             WHERE id = ?1",
        )?
        .execute(params![
            id,
            cut.saturating_add(1),
            bytes_to_sql(ByteCount::new(freed))?
        ])?;
    tracing::debug!(%session, first_kept = cut.saturating_add(1), freed, "pruned session log");
    Ok(())
}

/// Delete the ended sessions beyond the newest `max_ended_sessions`, with
/// their logs. Returns how many were deleted. Only sessions with an end
/// time count: every ended session has one (it comes from its final status
/// record), and requiring it keeps a row in any other state out of reach.
pub(crate) fn prune_ended(
    connection: &Connection,
    retention: &Retention,
) -> Result<usize, StoreError> {
    let deleted = connection
        .prepare_cached(
            "DELETE FROM sessions WHERE id IN (
                 SELECT id FROM sessions WHERE live = 0 AND ended_at IS NOT NULL
                 ORDER BY ended_at DESC, created_at DESC
                 LIMIT -1 OFFSET ?1)",
        )?
        .execute([retention.max_ended_sessions])?;
    if deleted > 0 {
        tracing::debug!(deleted, "pruned ended sessions");
    }
    Ok(deleted)
}

/// Sessions the log still shows as live, with their next sequence number.
pub(crate) fn live_sessions(
    connection: &Connection,
) -> Result<Vec<(SessionId, EventSeq)>, StoreError> {
    let mut statement =
        connection.prepare_cached("SELECT id, next_seq FROM sessions WHERE live = 1")?;
    let mut rows = statement.query([])?;
    let mut live = Vec::new();
    while let Some(row) = rows.next()? {
        let id = session_id_from_sql(&row.get::<_, String>(0)?)?;
        live.push((id, seq_from_sql(row.get(1)?)?));
    }
    Ok(live)
}
