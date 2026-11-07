//! The database schema and its forward-only migrations.
//!
//! The version lives in `SQLite`'s `PRAGMA user_version`. Each entry of
//! [`MIGRATIONS`] moves the schema up by one version; existing entries are
//! never edited once released, only appended to. A database newer than this
//! build is refused rather than guessed at.

use rusqlite::Connection;

use crate::error::StoreError;

/// Version 1: sessions and their event log.
///
/// - Times are nanoseconds since the Unix epoch (`i64`), so a record's
///   `at` round-trips exactly.
/// - `spec`, `status` and `event` hold `vessel-proto` types as JSON, decoded
///   only by this crate (ADR 0004).
/// - `live`, `first_seq`, `retained_bytes`, `ended_at` and
///   `harness_session` are kept up to date as records are appended, so
///   recovery, retention and listing never scan the log.
const V1: &str = "
CREATE TABLE sessions (
    id              TEXT    NOT NULL PRIMARY KEY,
    spec            TEXT    NOT NULL,
    status          TEXT    NOT NULL,
    live            INTEGER NOT NULL,
    created_at      INTEGER NOT NULL,
    ended_at        INTEGER,
    next_seq        INTEGER NOT NULL,
    first_seq       INTEGER NOT NULL,
    retained_bytes  INTEGER NOT NULL,
    harness_session TEXT
) STRICT;

CREATE INDEX sessions_live ON sessions (live) WHERE live = 1;
CREATE INDEX sessions_ended ON sessions (ended_at) WHERE live = 0;

CREATE TABLE events (
    session_id TEXT    NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    at         INTEGER NOT NULL,
    event      TEXT    NOT NULL,
    PRIMARY KEY (session_id, seq)
) STRICT, WITHOUT ROWID;
";

/// Version 2 (milestone 3): paired devices (ADR 0010; see `devices`).
///
/// - `secret_hash` is SHA-256 of the device's credential; the secret itself
///   is never stored.
/// - `scopes` is a JSON array of scope names.
/// - `revoked_at` is set instead of deleting the row, so a revoked device
///   is told `Revoked` and not `UnknownDevice`.
const V2: &str = "
CREATE TABLE devices (
    id          TEXT    NOT NULL PRIMARY KEY,
    label       TEXT    NOT NULL,
    secret_hash BLOB    NOT NULL,
    scopes      TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    last_seen   INTEGER,
    revoked_at  INTEGER
) STRICT;
";

/// Version 3 (milestone 4, protocol 5): which profile a session runs with
/// now and who created it (ADRs 0011, 0013).
///
/// - `profile` is the current profile's name, `NULL` for the harness's
///   default; it starts as the spec's and follows `ProfileChanged` records.
/// - `created_by` is a `Creator` as JSON; `NULL` (every session stored
///   before this version) reads as the owner, the only creator there was.
const V3: &str = "
ALTER TABLE sessions ADD COLUMN profile TEXT;
ALTER TABLE sessions ADD COLUMN created_by TEXT;
";

/// Version 4 (milestone 4, protocol 6): where a session came from when
/// another node of the fleet had a hand in it.
///
/// - `via` is the home a creating call came through, a `NodeId` as text.
/// - `origin` is a `SessionOrigin` as JSON (`remote` or `imported`).
/// - Both `NULL` for a session of this node's own, and for every session
///   stored before this version, which read as `None`.
const V4: &str = "
ALTER TABLE sessions ADD COLUMN via TEXT;
ALTER TABLE sessions ADD COLUMN origin TEXT;
";

/// Version 5 (milestone 5, protocol 7): the model route a session was
/// launched with.
///
/// - `model` is a `SessionModel` as JSON (`{ route, model }`); `NULL` for a
///   session on its harness's own provider, and for every session stored
///   before this version, which read as `None`.
const V5: &str = "
ALTER TABLE sessions ADD COLUMN model TEXT;
";

/// Every migration, oldest first. Index `n` takes the schema from version
/// `n` to `n + 1`.
const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5];

/// Bring the database to the newest schema.
pub(crate) fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    let found: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let supported = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
    if found > supported {
        return Err(StoreError::NewerSchema { found, supported });
    }
    let done = usize::try_from(found).unwrap_or(0);
    let pending = MIGRATIONS.get(done..).unwrap_or_default();
    if pending.is_empty() {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    for migration in pending {
        transaction.execute_batch(migration)?;
    }
    transaction.pragma_update(None, "user_version", supported)?;
    transaction.commit()?;
    tracing::info!(
        from = found,
        to = supported,
        "migrated the event log schema"
    );
    Ok(())
}

/// Settings every connection needs.
pub(crate) fn configure(connection: &Connection) -> Result<(), StoreError> {
    // Wait for a lock instead of failing at once; only the checkpoint and
    // migrations ever hold one long.
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// Settings for the writer connection: WAL so readers never block on it,
/// and `synchronous=NORMAL` so a commit does not `fsync` (WAL syncs at
/// checkpoints; a power cut may lose the last commits, a crash does not).
pub(crate) fn configure_writer(connection: &Connection) -> Result<(), StoreError> {
    configure(connection)?;
    let mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(%mode, "the event log could not switch to WAL mode");
    }
    connection.pragma_update(None, "synchronous", "normal")?;
    Ok(())
}
