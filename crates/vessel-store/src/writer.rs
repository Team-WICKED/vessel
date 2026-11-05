//! The writer thread: the only code that writes the database after
//! startup.
//!
//! It takes every operation already queued (up to [`MAX_BATCH`]), applies
//! each in its own savepoint inside one transaction, runs retention for the
//! sessions it touched, commits once, and only then answers each caller.
//! A caller's `append` therefore returns when its records are committed,
//! which is what lets the node say "everything before seq N is durable".
//!
//! It is a plain thread rather than a task because every `SQLite` call blocks;
//! the queue is a bounded tokio channel so async callers wait on it without
//! blocking.

use std::collections::HashSet;
use std::sync::Arc;

use rusqlite::{Connection, Transaction};
use tokio::sync::{mpsc, oneshot};
use vessel_proto::{EventRecord, SessionId, SessionInfo};

use crate::config::Retention;
use crate::error::StoreError;
use crate::log::{self, Appended};

/// Most operations committed in one transaction. Bounds how long one
/// commit takes (and so the latency of the first operation in it).
const MAX_BATCH: usize = 256;

/// Answers one write operation once its batch is committed.
pub(crate) type Done = oneshot::Sender<Result<(), StoreError>>;

/// One item on the writer's queue.
#[derive(Debug)]
pub(crate) enum WriteOp {
    /// Change the database.
    Write(Write),
    /// Commit what came before, checkpoint the WAL into the main file, and
    /// stop. Writes queued after it are dropped, which answers their callers
    /// with `Closed`.
    Close { done: oneshot::Sender<()> },
}

/// A change to the database.
#[derive(Debug)]
pub(crate) enum Write {
    /// Store a new session.
    Create { info: SessionInfo, done: Done },
    /// Append records to a session's log. Shared with the session runtime's
    /// ring and subscribers, so persisting them copies nothing.
    Append {
        session: SessionId,
        records: Vec<Arc<EventRecord>>,
        done: Done,
    },
}

/// Start the writer thread on `connection`.
pub(crate) fn spawn(
    connection: Connection,
    ops: mpsc::Receiver<WriteOp>,
    retention: Retention,
) -> Result<(), StoreError> {
    let _detached = std::thread::Builder::new()
        .name("vessel-store-writer".to_owned())
        .spawn(move || run(connection, ops, retention))
        .map_err(StoreError::Thread)?;
    Ok(())
}

fn run(mut connection: Connection, mut ops: mpsc::Receiver<WriteOp>, retention: Retention) {
    let mut batch = Vec::with_capacity(MAX_BATCH);
    let mut writes = Vec::with_capacity(MAX_BATCH);
    let mut closes = Vec::new();
    // Zero means every `Store` handle is gone.
    while ops.blocking_recv_many(&mut batch, MAX_BATCH) > 0 {
        for op in batch.drain(..) {
            match op {
                WriteOp::Write(write) if closes.is_empty() => writes.push(write),
                WriteOp::Write(_after_close) => {}
                WriteOp::Close { done } => closes.push(done),
            }
        }
        commit(&mut connection, writes.drain(..), &retention);
        if !closes.is_empty() {
            break;
        }
    }
    checkpoint(&connection);
    for done in closes {
        if done.send(()).is_err() {
            tracing::trace!("close caller went away");
        }
    }
    tracing::debug!("event log writer stopped");
}

/// Fold the WAL back into the database file so a stopped node leaves one
/// self-contained file behind.
fn checkpoint(connection: &Connection) {
    if let Err(error) = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)") {
        tracing::warn!(%error, "could not checkpoint the event log");
    }
}

/// Apply and commit one batch, then answer every caller in it.
fn commit(connection: &mut Connection, writes: impl Iterator<Item = Write>, retention: &Retention) {
    let mut answers: Vec<(Done, Result<(), StoreError>)> = Vec::new();
    let committed = connection.transaction().and_then(|mut transaction| {
        let mut touched = HashSet::new();
        let mut ended = false;
        for write in writes {
            answers.push(apply(&mut transaction, write, &mut touched, &mut ended));
        }
        retain(&mut transaction, &touched, ended, retention);
        transaction.commit()
    });
    let failed = committed.err().map(Arc::new);
    if let Some(error) = &failed {
        tracing::error!(%error, "committing to the event log failed");
    }
    for (done, result) in answers {
        let result = match (&failed, result) {
            (Some(error), Ok(())) => Err(StoreError::Commit(Arc::clone(error))),
            (_, result) => result,
        };
        if done.send(result).is_err() {
            tracing::trace!("store caller went away before its write committed");
        }
    }
}

/// Apply one write in its own savepoint, so a failing write leaves no
/// partial rows and does not affect the rest of the batch.
fn apply(
    transaction: &mut Transaction<'_>,
    write: Write,
    touched: &mut HashSet<SessionId>,
    ended: &mut bool,
) -> (Done, Result<(), StoreError>) {
    match write {
        Write::Create { info, done } => {
            let applied = isolated(transaction, |connection| {
                log::create_session(connection, &info)
            });
            (done, applied)
        }
        Write::Append {
            session,
            records,
            done,
        } => {
            let applied = isolated(transaction, |connection| {
                let Appended { ended: now_ended } =
                    log::append(connection, session, records.iter().map(Arc::as_ref))?;
                let _first = touched.insert(session);
                *ended |= now_ended;
                Ok(())
            });
            (done, applied)
        }
    }
}

fn isolated(
    transaction: &mut Transaction<'_>,
    operation: impl FnOnce(&Connection) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let savepoint = transaction.savepoint()?;
    operation(&savepoint)?;
    savepoint.commit()?;
    Ok(())
}

/// Retention for this batch. Failures are logged, not returned: keeping a
/// bit more history than configured must never fail the appends.
fn retain(
    transaction: &mut Transaction<'_>,
    touched: &HashSet<SessionId>,
    ended: bool,
    retention: &Retention,
) {
    for &session in touched {
        let pruned = isolated(transaction, |connection| {
            log::prune_session(connection, session, retention)
        });
        if let Err(error) = pruned {
            tracing::warn!(%session, %error, "could not prune session log");
        }
    }
    if ended {
        let pruned = isolated(transaction, |connection| {
            log::prune_ended(connection, retention).map(drop)
        });
        if let Err(error) = pruned {
            tracing::warn!(%error, "could not prune ended sessions");
        }
    }
}
