//! The session runtime the node uses.
//!
//! Owns: one actor task per session that owns its process (a PTY today, an
//! ACP agent with `vessel-agent`), numbers every event (`EventSeq`, gap-free
//! from zero), keeps a bounded in-memory ring of recent records, hands every
//! record to the node through a bounded durable channel, and broadcasts it
//! to live followers. Must not: persist anything (the node owns the store,
//! ADR 0001), speak the RPC protocol, or know harness specifics (it asks
//! `vessel-harness`).
//!
//! This API is the seam with the node (`docs/plans/backend-m1.md`, R6).
//!
//! ```text
//!             SessionHandle (cheap clone, many holders)
//!              | send(input) ----------------------> PTY input (bounded)
//!              | kill() -----> CancellationToken --> actor
//!              | follow(from) -> ring snapshot + broadcast receiver
//!              | info()/watch_info() <- watch <---- actor
//!
//!   actor (owns the process): output -> seq -> durable channel (node)
//!                                           -> ring + broadcast (followers)
//! ```
//!
//! [Decision Log]
//! - Purpose: many clients follow one session, attach late, and resume by
//!   sequence number; the node persists everything; a slow or crashed
//!   client must never lose or reorder records for others.
//! - Alternatives: Phase 0's shared `Arc<Session>` with mutexes around the
//!   child, backlog and stdin (any holder could mutate anything, and the
//!   backlog was the only history); one channel per subscriber fed by the
//!   actor (the actor would have to manage subscribers and their overflow).
//! - Choice: the actor alone mutates session state. Followers take a ring
//!   snapshot and a `broadcast` receiver under one short lock, so nothing
//!   falls between the two; a follower that lags behind the broadcast
//!   buffer gets `Lagged` from tokio and resumes from the store through the
//!   node. The durable channel is the only path that may slow the session:
//!   records are never dropped for the store, so when the node stops
//!   draining, the actor stops reading output, and the program stalls.
//! - Order per record: number it, wait for a durable slot, then append to
//!   the ring and broadcast in one step. A follower therefore never sees a
//!   record twice (snapshot and live are disjoint) and never sees a record
//!   the store will not get.
//! - Stopping: `kill` asks politely (`SIGHUP` for a PTY), then kills the
//!   whole process group after a grace period. The actor keeps reacting to
//!   `kill` even while it waits for a durable slot.

mod config;
mod error;
mod handle;
mod publish;
mod ring;
mod runtime;
mod stop;
mod terminal;

pub use config::{DEFAULT_DURABLE_CAPACITY, DEFAULT_RING_CAPACITY, RuntimeConfig};
pub use error::{InputKind, ModeKind, SessionError};
pub use handle::SessionHandle;
pub use ring::{Follow, Gap};
pub use runtime::{Created, SessionRuntime};
