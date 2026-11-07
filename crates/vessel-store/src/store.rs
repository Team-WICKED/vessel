//! The public handle to the event log.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};
use vessel_proto::{
    DeviceId, DeviceInfo, EventPage, EventRecord, EventSeq, SessionId, SessionInfo,
};

use crate::config::StoreConfig;
use crate::data_dir::DataDir;
use crate::devices::{CredentialHash, StoredDevice};
use crate::error::StoreError;
use crate::log;
use crate::reader::Reader;
use crate::writer::{self, Done, Write, WriteOp};
use crate::{files, recovery, schema};

/// The session event log. Cheap to clone; every clone talks to the same
/// writer thread and read pool.
#[derive(Debug, Clone)]
pub struct Store {
    writes: mpsc::Sender<WriteOp>,
    reader: Arc<Reader>,
}

/// A freshly opened store and what recovery found.
#[derive(Debug)]
pub struct Opened {
    /// The store.
    pub store: Store,
    /// Sessions that were live when the node last stopped, now marked
    /// `Interrupted` (their logs end with that status). Newest first.
    pub interrupted: Vec<SessionInfo>,
}

impl Store {
    /// Open (or create) the event log in `dir`, migrate it, and recover from
    /// the previous run: every session that was still live is closed with a
    /// final `Status{Interrupted}` record, then retention runs.
    ///
    /// # Errors
    /// The database cannot be opened or migrated (for example it was written
    /// by a newer Vessel), or the writer thread cannot start.
    pub async fn open(dir: &DataDir, config: StoreConfig) -> Result<Opened, StoreError> {
        let path = dir.database_path();
        let retention = config.retention;
        let opening = path.clone();
        let (connection, interrupted) = tokio::task::spawn_blocking(move || {
            files::private_database(&opening).map_err(StoreError::io(&opening))?;
            let mut connection = Connection::open(&opening)?;
            schema::configure_writer(&connection)?;
            schema::migrate(&mut connection)?;
            let interrupted = recovery::interrupt_live(&mut connection, Utc::now())?;
            let _pruned = log::prune_ended(&connection, &retention)?;
            Ok::<_, StoreError>((connection, interrupted))
        })
        .await
        .map_err(StoreError::Task)??;
        if !interrupted.is_empty() {
            tracing::info!(
                sessions = interrupted.len(),
                "marked sessions from the previous run as interrupted"
            );
        }

        let (writes, ops) = mpsc::channel(config.write_queue.get());
        writer::spawn(connection, ops, retention)?;
        let store = Self {
            writes,
            reader: Arc::new(Reader::new(path, config)),
        };
        Ok(Opened { store, interrupted })
    }

    /// Store a new session with an empty log.
    ///
    /// # Errors
    /// The id is already stored, or the write failed.
    pub async fn create_session(&self, info: SessionInfo) -> Result<(), StoreError> {
        self.write(|done| Write::Create {
            info: Box::new(info),
            done,
        })
        .await
    }

    /// Append records to a session's log; returns once they are committed.
    /// They must continue the log exactly (next sequence number, no gaps).
    /// Records are taken as `Arc`s, the form the session runtime shares them
    /// in, so persisting them clones nothing.
    ///
    /// Status, end time and harness session id of the stored session follow
    /// from the records (`Status` and `Agent{Ready}` events).
    ///
    /// # Errors
    /// The session is unknown, the records do not continue its log, or the
    /// write failed. A failed append leaves the log unchanged.
    pub async fn append(
        &self,
        session: SessionId,
        records: Vec<Arc<EventRecord>>,
    ) -> Result<(), StoreError> {
        self.write(|done| Write::Append {
            session,
            records,
            done,
        })
        .await
    }

    /// A page of a session's log; see `session.read` in the protocol.
    ///
    /// # Errors
    /// The session is unknown, or the read failed.
    pub async fn read_events(
        &self,
        session: SessionId,
        from: EventSeq,
        limit: u32,
    ) -> Result<EventPage, StoreError> {
        self.read(move |reader| reader.read_events(session, from, limit))
            .await
    }

    /// One stored session.
    ///
    /// # Errors
    /// The read failed.
    pub async fn session(&self, id: SessionId) -> Result<Option<SessionInfo>, StoreError> {
        self.read(move |reader| reader.session(id)).await
    }

    /// Every stored session, newest first.
    ///
    /// # Errors
    /// The read failed.
    pub async fn sessions(&self) -> Result<Vec<SessionInfo>, StoreError> {
        self.read(Reader::sessions).await
    }

    /// Store a newly paired device with its credential's hash.
    ///
    /// # Errors
    /// The id is already stored, or the write failed.
    pub async fn add_device(
        &self,
        device: DeviceInfo,
        credential: CredentialHash,
    ) -> Result<(), StoreError> {
        self.write(|done| Write::AddDevice {
            device,
            credential,
            done,
        })
        .await
    }

    /// Note that `device` authenticated at `at` (`last_seen`).
    ///
    /// # Errors
    /// The write failed.
    pub async fn device_seen(&self, device: DeviceId, at: DateTime<Utc>) -> Result<(), StoreError> {
        self.write(|done| Write::DeviceSeen { device, at, done })
            .await
    }

    /// Revoke `device` at `at`. It stays listed, with `revoked_at` set;
    /// revoking it again keeps the first time.
    ///
    /// # Errors
    /// [`StoreError::UnknownDevice`] if there is no such device, or the
    /// write failed.
    pub async fn revoke_device(
        &self,
        device: DeviceId,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.write(|done| Write::RevokeDevice { device, at, done })
            .await
    }

    /// Every paired device, revoked ones included, oldest first.
    ///
    /// # Errors
    /// The read failed.
    pub async fn devices(&self) -> Result<Vec<DeviceInfo>, StoreError> {
        self.read(Reader::devices).await
    }

    /// One device with its credential hash, for authentication.
    ///
    /// # Errors
    /// The read failed.
    pub async fn device(&self, id: DeviceId) -> Result<Option<StoredDevice>, StoreError> {
        self.read(move |reader| reader.device(id)).await
    }

    /// Commit everything queued so far, fold the WAL into the database file
    /// and stop the writer. Later writes through any clone fail with
    /// [`StoreError::Closed`]; reads keep working.
    pub async fn close(&self) {
        let (done, closed) = oneshot::channel();
        if self.writes.send(WriteOp::Close { done }).await.is_ok() && closed.await.is_err() {
            tracing::debug!("event log writer stopped before acknowledging close");
        }
    }

    async fn write(&self, write: impl FnOnce(Done) -> Write) -> Result<(), StoreError> {
        let (done, committed) = oneshot::channel();
        self.writes
            .send(WriteOp::Write(write(done)))
            .await
            .map_err(|_closed| StoreError::Closed)?;
        committed.await.map_err(|_dropped| StoreError::Closed)?
    }

    async fn read<T: Send + 'static>(
        &self,
        read: impl FnOnce(&Reader) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let reader = Arc::clone(&self.reader);
        tokio::task::spawn_blocking(move || read(&reader))
            .await
            .map_err(StoreError::Task)?
    }
}
