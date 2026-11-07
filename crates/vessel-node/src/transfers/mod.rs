//! Transfers (protocol 6, plan F4): a session's conversation or a
//! profile's memory, snapshot into a staging store on the source node
//! ([`export`]), read chunk by chunk and closed by whoever opened it
//! ([`Transfers`]), and pulled, placed and resumed on the target
//! ([`import`]).
//!
//! ```text
//!  session.export / memory.export ──▶ export ──snapshot──▶ <data_dir>/transfers/<id>/  ◀── transfer.read / close
//!  session.import / memory.import ──▶ import ── Source ──▶ pull into transfers/incoming-<id>/ ──place──▶ the profile's home
//!                                                  │ (this node, or another through the envelope)
//!                                                  └── start: session.create resuming it, origin imported
//! ```
//!
//! [Decision Log]
//! - Purpose: a conversation or a memory moves between profiles and
//!   nodes as files (ADR 0012, "history and memory on demand"); which
//!   files is the harness's knowledge (`vessel-harness`), how they are
//!   copied and written is `vessel-fleet`'s (`snapshot`, `place`). This
//!   module decides who may read a transfer and for how long, and puts
//!   the steps of an import in order.
//! - A transfer belongs to whoever opened it ([`Holder`]: the owner, a
//!   device, or an agent's session), whatever scopes it holds: reading or
//!   closing it as anyone else answers `not_found`, as an unknown id does,
//!   so ids cannot be probed. A `TransferId` is a `UUIDv4` (122 random
//!   bits rather than the plan's 128): it is not a secret, the holder
//!   check is what keeps a transfer to its opener. A delivered call's
//!   holder is the delivering device together with the origin
//!   (`Holder::Delegated`, protocol 6).
//! - Lifetime: closed by `transfer.close`, after [`IDLE`] without a read,
//!   or at restart (the staging directory is emptied before the node
//!   serves anything, `clear`). At most [`MAX_OPEN`] at once, so a caller
//!   that never closes cannot fill the disk with snapshots of up to 2 GiB
//!   each; the next one is refused with `unavailable` until one closes or
//!   expires.
//! - The import's source is a small trait ([`import::Source`]): this node
//!   (a session or memory of another profile here, `Local`), or another
//!   fleet node through the envelope (`Remote`, on `NodeService::pass_on`,
//!   the route `fleet.forward` takes), as the caller of the import.
//! - Alternatives: hard links for a same-node copy (`link_session` does
//!   that for a move between profiles; a copy that later diverges must not
//!   share the file); holding the snapshot in memory (up to 2 GiB).

pub(crate) mod export;
pub(crate) mod import;

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_fleet::BlobStore;
use vessel_harness::{LinkError, MemoryFilesError, RebaseError, SessionFilesError};
use vessel_proto::{
    BlobChunk, BlobHash, BundleFile, BundlePath, DeviceId, ErrorCode, NodeId, OriginBy,
    ReadTransfer, SessionId, TransferId, WireError,
};
use vessel_store::DataDir;

use crate::principal::Principal;

/// How long a transfer stays open without a read (protocol 6).
pub(crate) const IDLE: Duration = Duration::from_secs(10 * 60);

/// The most transfers open at once on a node.
pub(crate) const MAX_OPEN: usize = 16;

/// How often expired transfers are looked for.
const SWEEP: Duration = Duration::from_secs(30);

/// Who opened a transfer: a principal without its scopes, which a
/// transfer's reads do not depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holder {
    /// The machine's own clients.
    Owner,
    /// A paired device.
    Device(DeviceId),
    /// A session's agent.
    Agent {
        /// The node of its session.
        node: NodeId,
        /// Its session.
        session: SessionId,
    },
    /// A call delivered from another node (protocol 6): the delivering
    /// device together with the origin, as the contract says, so neither
    /// another origin through the same home nor the home on its own reads
    /// what this origin opened.
    Delegated {
        /// The device that delivered it.
        device: DeviceId,
        /// The node the call started on.
        node: NodeId,
        /// Who started it there.
        by: OriginBy,
    },
}

impl Holder {
    /// The holder `principal` is.
    pub(crate) const fn of(principal: &Principal) -> Self {
        match principal {
            Principal::Owner => Self::Owner,
            Principal::Device { id, .. } => Self::Device(*id),
            Principal::Agent { node, session, .. } => Self::Agent {
                node: *node,
                session: *session,
            },
            Principal::Delegated { device, origin, .. } => Self::Delegated {
                device: *device,
                node: origin.node,
                by: origin.by,
            },
        }
    }
}

/// Why a transfer could not be opened, read, pulled or placed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransferError {
    /// A terminal session: its harness keeps the conversation in a form
    /// Vessel does not resume.
    #[error("terminal sessions cannot be copied; only structured ones")]
    Terminal,
    /// The session's agent has not reported its conversation yet.
    #[error("the session's agent is still starting; there is no conversation to copy yet")]
    NotReady,
    /// A turn is running and the export was not forced.
    #[error(
        "a turn is running; wait for it to end, or force the export (it then copies the \
         conversation as far as it is written)"
    )]
    TurnInProgress,
    /// The session's files could not be found.
    #[error(transparent)]
    Files(#[from] SessionFilesError),
    /// The session's files are not what a copy may take.
    #[error(transparent)]
    Link(#[from] LinkError),
    /// The profile's memory could not be listed.
    #[error(transparent)]
    Memory(#[from] MemoryFilesError),
    /// A path could not be renamed for the target's working directory.
    #[error(transparent)]
    Rebase(#[from] RebaseError),
    /// The files could not be snapshot (over a limit, a name that cannot
    /// travel, a read failure).
    #[error(transparent)]
    Snapshot(#[from] vessel_fleet::SnapshotError),
    /// The files could not be placed.
    #[error(transparent)]
    Place(#[from] vessel_fleet::PlaceError),
    /// A manifest received from a source is over a limit.
    #[error("{what} is {size} bytes, over the limit of {limit} bytes")]
    Oversized {
        /// What: a file's path, or "the transfer".
        what: String,
        /// Its size.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// No open transfer of that id for this caller (or none at all).
    #[error("no open transfer {0}")]
    Unknown(TransferId),
    /// The transfer holds no file with that hash.
    #[error("transfer {transfer} has no file with hash {hash}")]
    NoFile {
        /// The transfer.
        transfer: TransferId,
        /// The hash asked for.
        hash: BlobHash,
    },
    /// [`MAX_OPEN`] transfers are open.
    #[error("{0} transfers are open on this node; close one, or wait for one to expire")]
    TooMany(usize),
    /// A working directory that is not absolute.
    #[error("{} is not an absolute directory", .0.display())]
    RelativeDirectory(PathBuf),
    /// A session cannot start in a directory this node does not have.
    #[error("{} is not a directory on this node", .0.display())]
    NoDirectory(PathBuf),
    /// A project to import into, with all of a profile's memory to copy.
    #[error("a project to import memory into needs a project to copy it from")]
    ProjectWithoutSource,
    /// The import would put a session where it already is.
    #[error("that conversation is already in this profile and directory")]
    AlreadyThere,
    /// The target profile's harness home does not exist: the harness has
    /// never run there, and an import does not make one.
    #[error(
        "profile `{profile}` has no harness home on this node yet ({}); run or log in to the \
         harness under it once first",
        home.display()
    )]
    NoHome {
        /// The profile.
        profile: vessel_proto::ProfileName,
        /// Where its home would be.
        home: PathBuf,
    },
    /// The source answered with another kind of transfer.
    #[error("the source answered with a {0} transfer")]
    WrongKind(&'static str),
    /// The staging directory could not be used.
    #[error("the transfer's staging directory: {0}")]
    Staging(#[source] io::Error),
    /// Fetching from the source failed (its answer, or the store).
    #[error("fetching a file: {0}")]
    Fetch(String),
    /// A blocking task failed.
    #[error("a node task failed")]
    Task(#[source] tokio::task::JoinError),
}

impl TransferError {
    /// The wire code.
    pub(crate) const fn code(&self) -> ErrorCode {
        use vessel_fleet::{PlaceError, SnapshotError};
        match self {
            Self::Terminal
            | Self::Files(SessionFilesError::Unsupported { .. })
            | Self::Memory(MemoryFilesError::Unsupported { .. }) => ErrorCode::Unsupported,
            Self::NotReady
            | Self::TurnInProgress
            | Self::AlreadyThere
            | Self::NoHome { .. }
            | Self::Files(
                SessionFilesError::NotFound { .. } | SessionFilesError::Ambiguous { .. },
            )
            | Self::Link(
                LinkError::Refused { .. }
                | LinkError::SymbolicLink { .. }
                | LinkError::Conflict { .. },
            )
            | Self::Snapshot(SnapshotError::Path { .. })
            | Self::Place(
                PlaceError::Conflict { .. }
                | PlaceError::Blocked { .. }
                | PlaceError::Changed { .. },
            ) => ErrorCode::Conflict,
            Self::Snapshot(
                SnapshotError::FileTooLarge { .. } | SnapshotError::TotalTooLarge { .. },
            )
            | Self::Oversized { .. }
            | Self::RelativeDirectory(_)
            | Self::NoDirectory(_)
            | Self::ProjectWithoutSource
            | Self::Rebase(_)
            | Self::Files(
                SessionFilesError::InvalidId { .. } | SessionFilesError::InvalidDirectory { .. },
            )
            | Self::Memory(MemoryFilesError::InvalidDirectory { .. })
            | Self::Place(PlaceError::Path { .. }) => ErrorCode::InvalidRequest,
            Self::Unknown(_) | Self::NoFile { .. } => ErrorCode::NotFound,
            Self::TooMany(_) => ErrorCode::Unavailable,
            Self::Files(SessionFilesError::Io { .. })
            | Self::Link(LinkError::Io { .. })
            | Self::Memory(MemoryFilesError::Io { .. })
            | Self::Snapshot(SnapshotError::Io { .. })
            | Self::Place(PlaceError::Io { .. } | PlaceError::DamagedBlob { .. })
            | Self::WrongKind(_)
            | Self::Staging(_)
            | Self::Fetch(_)
            | Self::Task(_) => ErrorCode::Internal,
        }
    }
}

/// One open transfer.
#[derive(Debug)]
struct Open {
    holder: Holder,
    blobs: BlobStore,
    hashes: BTreeSet<BlobHash>,
    /// When it was opened or last read.
    used: Mutex<Instant>,
}

impl Open {
    fn touch(&self) {
        *self.used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(*self.used.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The node's open transfers, and their staging directory.
#[derive(Debug)]
pub(crate) struct Transfers {
    root: PathBuf,
    /// A `std` mutex: held for a lookup or an insert, never across
    /// `.await`.
    open: Mutex<HashMap<TransferId, Arc<Open>>>,
    idle: Duration,
}

/// A staging store, made for a transfer about to open (or a pull about to
/// start); its directory is removed unless the transfer opens.
#[derive(Debug)]
pub(crate) struct Staged {
    pub(crate) id: TransferId,
    pub(crate) blobs: BlobStore,
}

/// `<data_dir>/transfers`.
pub(crate) fn root(data_dir: &DataDir) -> PathBuf {
    data_dir.root().join("transfers")
}

/// Empty the staging directory: what a previous run left is expired
/// (protocol 6, "at restart"). Blocks; called before the node serves.
pub(crate) fn clear(data_dir: &DataDir) {
    let root = root(data_dir);
    match std::fs::remove_dir_all(&root) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            tracing::warn!(path = %root.display(), %error, "could not empty the transfers' staging directory");
        }
        Ok(()) | Err(_) => {}
    }
}

impl Transfers {
    pub(crate) fn new(root: PathBuf, idle: Duration) -> Self {
        Self {
            root,
            open: Mutex::new(HashMap::new()),
            idle,
        }
    }

    fn open(&self) -> std::sync::MutexGuard<'_, HashMap<TransferId, Arc<Open>>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A fresh staging store (`incoming` for a pull's), refused while
    /// [`MAX_OPEN`] transfers are open. Blocks.
    pub(crate) fn stage(&self, incoming: bool) -> Result<Staged, TransferError> {
        if !incoming && self.open().len() >= MAX_OPEN {
            return Err(TransferError::TooMany(MAX_OPEN));
        }
        let id = TransferId::random();
        let name = if incoming {
            format!("incoming-{id}")
        } else {
            id.to_string()
        };
        let blobs = BlobStore::open(self.root.join(name)).map_err(TransferError::Staging)?;
        Ok(Staged { id, blobs })
    }

    /// Open `staged` for `holder`, holding `files`; refused (and its
    /// directory removed) if [`MAX_OPEN`] opened meanwhile.
    pub(crate) fn commit<'a>(
        &self,
        staged: Staged,
        holder: Holder,
        files: impl IntoIterator<Item = &'a BundleFile>,
    ) -> Result<TransferId, TransferError> {
        let Staged { id, blobs } = staged;
        let mut open = self.open();
        if open.len() >= MAX_OPEN {
            drop(open);
            discard(blobs.root());
            return Err(TransferError::TooMany(MAX_OPEN));
        }
        let transfer = Open {
            holder,
            blobs,
            hashes: files.into_iter().map(|file| file.hash).collect(),
            used: Mutex::new(Instant::now()),
        };
        drop(open.insert(id, Arc::new(transfer)));
        Ok(id)
    }

    /// `holder`'s transfer `id`.
    fn held(&self, holder: Holder, id: TransferId) -> Result<Arc<Open>, TransferError> {
        self.open()
            .get(&id)
            .filter(|open| open.holder == holder)
            .cloned()
            .ok_or(TransferError::Unknown(id))
    }

    /// `transfer.read`.
    pub(crate) async fn read(
        &self,
        holder: Holder,
        request: ReadTransfer,
    ) -> Result<BlobChunk, WireError> {
        let open = self
            .held(holder, request.transfer)
            .map_err(|error| wire(&error))?;
        if !open.hashes.contains(&request.hash) {
            return Err(wire(&TransferError::NoFile {
                transfer: request.transfer,
                hash: request.hash,
            }));
        }
        open.touch();
        crate::fleet::serve(open.blobs.clone(), request.hash, request.offset).await
    }

    /// `transfer.close`.
    pub(crate) async fn close(&self, holder: Holder, id: TransferId) -> Result<(), TransferError> {
        let removed = {
            let mut open = self.open();
            match open.get(&id) {
                Some(transfer) if transfer.holder == holder => open.remove(&id),
                Some(_) | None => None,
            }
        }
        .ok_or(TransferError::Unknown(id))?;
        let root = removed.blobs.root().to_owned();
        tokio::task::spawn_blocking(move || discard(&root))
            .await
            .map_err(TransferError::Task)?;
        Ok(())
    }

    /// Close every transfer idle for longer than allowed at `now`. Blocks.
    fn expire(&self, now: Instant) {
        let expired: Vec<Arc<Open>> = {
            let mut open = self.open();
            let ids: Vec<TransferId> = open
                .iter()
                .filter(|(_, transfer)| transfer.idle_for(now) > self.idle)
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| open.remove(id)).collect()
        };
        for transfer in expired {
            tracing::info!(path = %transfer.blobs.root().display(), "a transfer expired");
            discard(transfer.blobs.root());
        }
    }

    /// Close expired transfers every little while until `stop`.
    pub(crate) fn sweep(self: &Arc<Self>, tasks: &TaskTracker, stop: &CancellationToken) {
        let transfers = Arc::clone(self);
        let stop = stop.clone();
        let every = SWEEP.min(self.idle);
        drop(tasks.spawn(async move {
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(every) => {}
                }
                let transfers = Arc::clone(&transfers);
                if let Err(error) =
                    tokio::task::spawn_blocking(move || transfers.expire(Instant::now())).await
                {
                    tracing::warn!(%error, "expiring transfers failed");
                }
            }
        }));
    }
}

/// Remove a staging directory; failures are logged (the next start
/// empties the staging directory anyway).
pub(crate) fn discard(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            tracing::warn!(path = %dir.display(), %error, "could not remove a transfer's files");
        }
        Ok(()) | Err(_) => {}
    }
}

/// The error on the wire, with its causes.
pub(crate) fn wire(error: &TransferError) -> WireError {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    WireError::new(error.code(), message)
}

/// A manifest's files within protocol 6's limits (what a pull checks
/// before fetching anything).
pub(crate) fn check_limits<'a>(
    files: impl IntoIterator<Item = (&'a BundlePath, &'a BundleFile)>,
) -> Result<(), TransferError> {
    let mut total = 0_u64;
    for (path, file) in files {
        if file.size > vessel_proto::MAX_TRANSFER_FILE {
            return Err(TransferError::Oversized {
                what: path.to_string(),
                size: file.size,
                limit: vessel_proto::MAX_TRANSFER_FILE,
            });
        }
        total = total.saturating_add(file.size);
    }
    if total > vessel_proto::MAX_TRANSFER_TOTAL {
        return Err(TransferError::Oversized {
            what: "the transfer".to_owned(),
            size: total,
            limit: vessel_proto::MAX_TRANSFER_TOTAL,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_one(transfers: &Transfers, holder: Holder, text: &[u8]) -> (TransferId, BundleFile) {
        let staged = transfers.stage(false).unwrap();
        let file = BundleFile {
            hash: staged.blobs.insert(text).unwrap(),
            size: u64::try_from(text.len()).unwrap(),
            executable: false,
        };
        let id = transfers.commit(staged, holder, [&file]).unwrap();
        (id, file)
    }

    #[tokio::test]
    async fn only_whoever_opened_a_transfer_reads_or_closes_it() {
        let temp = tempfile::tempdir().unwrap();
        let transfers = Transfers::new(temp.path().join("transfers"), IDLE);
        let me = Holder::Device(DeviceId::random());
        let (id, file) = open_one(&transfers, me, b"hello");
        let read = |holder, hash| {
            transfers.read(
                holder,
                ReadTransfer {
                    transfer: id,
                    hash,
                    offset: 0,
                },
            )
        };
        assert_eq!(read(me, file.hash).await.unwrap().data.as_ref(), b"hello");
        let agent = Holder::Agent {
            node: NodeId::random(),
            session: SessionId::random(),
        };
        for other in [Holder::Owner, Holder::Device(DeviceId::random()), agent] {
            let refused = read(other, file.hash).await.unwrap_err();
            assert_eq!(refused.code, ErrorCode::NotFound, "{other:?}");
            assert!(matches!(
                transfers.close(other, id).await,
                Err(TransferError::Unknown(_))
            ));
        }
        let elsewhere = BlobHash::from_bytes([7; 32]);
        assert_eq!(
            read(me, elsewhere).await.unwrap_err().code,
            ErrorCode::NotFound
        );

        let dir = temp.path().join("transfers").join(id.to_string());
        assert!(dir.is_dir());
        transfers.close(me, id).await.unwrap();
        assert!(!dir.exists(), "its files go with it");
        assert_eq!(
            read(me, file.hash).await.unwrap_err().code,
            ErrorCode::NotFound
        );
    }

    #[test]
    fn transfers_are_limited_in_number_and_expire_when_idle() {
        let temp = tempfile::tempdir().unwrap();
        let transfers = Transfers::new(temp.path().join("transfers"), IDLE);
        for index in 0..MAX_OPEN {
            let _opened = open_one(&transfers, Holder::Owner, &index.to_le_bytes());
        }
        assert!(matches!(
            transfers.stage(false),
            Err(TransferError::TooMany(MAX_OPEN))
        ));
        // A pull's incoming store is not an open transfer.
        assert!(transfers.stage(true).is_ok());

        transfers.expire(Instant::now());
        assert_eq!(transfers.open().len(), MAX_OPEN, "not idle yet");
        let later = Instant::now()
            .checked_add(IDLE.saturating_add(Duration::from_secs(1)))
            .unwrap();
        transfers.expire(later);
        assert!(transfers.open().is_empty());
        let left: Vec<_> = std::fs::read_dir(temp.path().join("transfers"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "only the incoming store: {left:?}");
        assert!(transfers.stage(false).is_ok());
    }
}
