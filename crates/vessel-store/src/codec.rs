//! Conversions between `vessel-proto` values and `SQLite` columns. Every
//! column the store reads is decoded here, once.

use chrono::{DateTime, Utc};
use rusqlite::Row;
use vessel_proto::{
    EventRecord, EventSeq, HarnessSessionId, SessionEvent, SessionId, SessionInfo, SessionSpec,
    SessionStatus,
};

use crate::config::ByteCount;
use crate::error::{Column, Quantity, StoreError};

pub(crate) fn seq_to_sql(seq: EventSeq) -> Result<i64, StoreError> {
    i64::try_from(seq.get()).map_err(|_too_big| StoreError::OutOfRange(Quantity::Seq))
}

pub(crate) fn seq_from_sql(value: i64) -> Result<EventSeq, StoreError> {
    u64::try_from(value)
        .map(EventSeq::new)
        .map_err(|_negative| StoreError::OutOfRange(Quantity::Seq))
}

pub(crate) fn time_to_sql(time: DateTime<Utc>) -> Result<i64, StoreError> {
    time.timestamp_nanos_opt()
        .ok_or(StoreError::OutOfRange(Quantity::Time))
}

pub(crate) const fn time_from_sql(nanos: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_nanos(nanos)
}

pub(crate) fn bytes_to_sql(bytes: ByteCount) -> Result<i64, StoreError> {
    i64::try_from(bytes.get()).map_err(|_too_big| StoreError::OutOfRange(Quantity::Bytes))
}

pub(crate) fn bytes_from_sql(value: i64) -> Result<ByteCount, StoreError> {
    u64::try_from(value)
        .map(ByteCount::new)
        .map_err(|_negative| StoreError::OutOfRange(Quantity::Bytes))
}

pub(crate) fn session_id_from_sql(text: &str) -> Result<SessionId, StoreError> {
    text.parse().map_err(|source| StoreError::CorruptId {
        text: text.to_owned(),
        source,
    })
}

pub(crate) fn encode<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(StoreError::Encode)
}

fn decode<T: serde::de::DeserializeOwned>(
    session: SessionId,
    column: Column,
    text: &str,
) -> Result<T, StoreError> {
    serde_json::from_str(text).map_err(|source| StoreError::Corrupt {
        session,
        column,
        source,
    })
}

/// The columns [`session_from_row`] expects, in order.
pub(crate) const SESSION_COLUMNS: &str =
    "id, spec, status, created_at, ended_at, next_seq, harness_session";

pub(crate) fn session_from_row(row: &Row<'_>) -> Result<SessionInfo, StoreError> {
    let id = session_id_from_sql(&row.get::<_, String>(0)?)?;
    let spec: SessionSpec = decode(id, Column::Spec, &row.get::<_, String>(1)?)?;
    let status: SessionStatus = decode(id, Column::Status, &row.get::<_, String>(2)?)?;
    // `profile` and `created_by` are not stored yet: until schema 3
    // (milestone 4 Track H) a stored session reads as running the profile it
    // asked for and created by the owner, which is all a node could do
    // before protocol 5.
    let mut info = SessionInfo::new(id, spec, time_from_sql(row.get(3)?));
    info.status = status;
    info.ended_at = row.get::<_, Option<i64>>(4)?.map(time_from_sql);
    info.next_seq = seq_from_sql(row.get(5)?)?;
    info.harness_session = row.get::<_, Option<String>>(6)?.map(HarnessSessionId::new);
    Ok(info)
}

/// A stored record and its stored size; the size counts toward page and
/// retention limits.
pub(crate) fn record_from_row(
    session: SessionId,
    row: &Row<'_>,
) -> Result<(EventRecord, usize), StoreError> {
    let text: String = row.get(2)?;
    let event: SessionEvent = decode(session, Column::Event, &text)?;
    let record = EventRecord {
        seq: seq_from_sql(row.get(0)?)?,
        at: time_from_sql(row.get(1)?),
        event,
    };
    Ok((record, text.len()))
}
