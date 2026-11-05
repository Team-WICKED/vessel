//! Process primitives: what to run, and how to stop all of it.
//!
//! This crate owns three things and nothing else:
//!
//! - [`CommandSpec`]: a plain-data description of a command (program, args,
//!   environment changes, working directory). `vessel-harness` builds them;
//!   `vessel-pty` and [`spawn`] run them. It carries no harness knowledge.
//! - [`spawn`]: start a command with piped stdio as the leader of a new
//!   process group, returning a [`Child`] that stops the whole group.
//! - [`ProcessGroup`] (Unix): signal every process in a group, for runners
//!   such as `vessel-pty` that spawn through another library.
//!
//! It must not know about sessions, harnesses or the wire protocol, so it
//! depends on nothing from Vessel.
//!
//! [Decision Log]
//! - Purpose: a harness starts its own children (MCP servers, shells,
//!   language servers). Killing only the harness leaves those running and,
//!   worse, holding the session's output pipe or PTY open forever.
//! - Alternatives: `tokio`'s `kill_on_drop` (kills only the direct child);
//!   Linux-only subreapers or cgroups (not portable, need privileges);
//!   Windows job objects (need `unsafe` FFI, which the workspace forbids, or
//!   another dependency).
//! - Choice: every child is the leader of its own process group, and stopping
//!   it signals the group (`killpg`). On Windows there are no process groups
//!   in this sense; the direct child is terminated and its descendants may
//!   survive. That gap is accepted for milestone 1 and documented on
//!   [`Child::signal`].
//! - Constraint: after the leader is reaped its group id can only be reused
//!   once every member is gone, so sweeping the group right after the leader
//!   exits reaches exactly its leftovers. The one remaining race (the group
//!   empties and a brand-new process takes the same id as a group leader
//!   within microseconds) is accepted; closing it needs pidfds or a
//!   subreaper.

mod child;
mod error;
#[cfg(unix)]
mod group;
mod signal;
mod spec;

pub use child::{Child, Spawned, spawn};
pub use error::{InvalidGroup, SignalError, SpawnError, Stream, WaitError};
#[cfg(unix)]
pub use group::ProcessGroup;
pub use signal::Signal;
pub use spec::{CommandSpec, EnvValue};
