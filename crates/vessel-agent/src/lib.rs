//! Vessel's ACP client: structured sessions.
//!
//! Drives a harness (or its adapter) over the Agent Client Protocol on
//! stdio and turns what it reports into Vessel's own timeline
//! ([`vessel_proto::AgentEvent`]). ACP types never leave this crate (ADR
//! 0003): an ACP version bump changes this crate, not the wire format.
//!
//! Modules, by responsibility:
//!
//! - `process`: [`AgentProcess`], spawning through `vessel-proc` (own
//!   process group) and starting the tasks below;
//! - `driver`: the ACP connection and the session state machine
//!   (handshake, prompt, cancel, permission round trip);
//! - `handshake`: `initialize`, `session/new` or `session/load`, model;
//! - `model`: picking the requested model among ACP config options;
//! - `permissions`: pending permission requests, first answer wins;
//! - `mapper`: pure ACP-to-Vessel conversions, tested with fixtures;
//! - `stderr`: harness stderr lines as `HarnessLog` events;
//! - `input`, `output`: the public command and event ends.
//!
//! Client capabilities are empty: no `fs`, no `terminal`. Agents use their
//! own tools, as they do in a terminal, and Vessel does not re-implement
//! them.
//!
//! Known gaps, waiting on the wire contract: ACP's user-message chunks
//! (replayed by `session/load`) and `raw_input` carried by tool call
//! *updates* have no `AgentEvent` field, so they are not forwarded.
//!
//! [Decision Log]
//! - Purpose: one client for every structured harness (ADR 0003).
//! - Alternatives: the SDK's `ActiveSession` helper (it buffers updates in
//!   an unbounded channel and hides permission requests behind its own
//!   flow); the SDK's `process` feature (`AcpAgent` spawns without a
//!   process group, so a harness's MCP servers would outlive it).
//! - Choice: raw `Client.v1()` handlers that forward into one bounded
//!   channel consumed by a single driver loop (see `driver`), over the
//!   child's piped stdio from `vessel-proc`.
//! - Cost: the session state machine is written here (~300 lines), not
//!   borrowed from the SDK.

mod driver;
mod error;
mod handshake;
mod input;
mod mapper;
mod model;
mod output;
mod permissions;
mod process;
mod stderr;

pub use error::{AgentError, Disconnect, InputError};
pub use handshake::AgentSetup;
pub use input::AgentInput;
pub use output::{AgentOutput, AgentOutputs};
pub use process::AgentProcess;
pub use vessel_proc::Signal;
