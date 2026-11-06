//! A node's durable state: its data directory and the session event log.
//!
//! - [`DataDir`] is the directory layout: `node.id` (stable identity),
//!   `token` (the bearer token clients present, ADR 0005), `node.key` (the
//!   node's secret key, ADR 0009), `vessel.db`, and the paths milestone 2
//!   adds (local socket, `bin/`, `logs/`).
//! - [`NodeLock`] keeps one node per data directory and records which
//!   process that is ([`NodeOwner`]).
//! - [`Store`] is the event log (ADR 0004): one `SQLite` database holding each
//!   session's metadata and every [`EventRecord`] it produced, so a client
//!   can resume from any sequence number even after the node restarts.
//!
//! This crate is the only place that knows the on-disk formats. It decodes
//! rows into `vessel-proto` types once, here, and nothing outside sees SQL or
//! stored JSON.
//!
//! [Decision Log]
//! - Purpose: history that survives restarts, paged reads by sequence
//!   number, bounded disk use, and no blocking I/O on async threads.
//! - Choice: `SQLite` in WAL mode (ADR 0004 compares it with JSONL files). One
//!   writer thread owns the only write connection and is fed by a bounded
//!   queue; it commits each batch of queued operations in one transaction
//!   (no `fsync` per event: WAL with `synchronous=NORMAL` syncs at
//!   checkpoints), isolating each operation in a savepoint so one bad append
//!   cannot fail its neighbours. Reads run on the blocking pool with their
//!   own connections, which WAL lets proceed while the writer commits.
//! - The `sessions` row is a projection of the log: status, end time and
//!   the harness session id are derived from the records as they are
//!   appended, so the summary can never disagree with the history.
//! - Cost: a C build step (bundled `SQLite`), and writes are acknowledged per
//!   batch commit rather than immediately.
//!
//! [`EventRecord`]: vessel_proto::EventRecord

mod codec;
mod config;
mod data_dir;
mod error;
mod files;
mod lock;
mod log;
mod node_key;
mod reader;
mod recovery;
mod schema;
mod store;
mod token;
mod writer;

pub use config::{ByteCount, Retention, StoreConfig};
pub use data_dir::DataDir;
pub use error::{Column, Quantity, StoreError};
pub use lock::{NodeLock, NodeOwner, StartLock};
pub use node_key::{NODE_KEY_BYTES, NodeKeySecret};
pub use store::{Opened, Store};
pub use token::{InvalidToken, NodeToken};
