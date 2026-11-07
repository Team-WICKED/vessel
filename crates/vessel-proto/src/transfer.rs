//! Transfers (protocol 6, plan F4): a session's harness files or a
//! profile's memory, copied from one node to another by the target pulling.
//!
//! [Decision Log]
//! - Purpose: continue a conversation on another node, or bring a
//!   project's memory along, without the person copying files by hand.
//! - Constraint: a target can reach other nodes only through its home
//!   (`fleet.forward`), and a live transcript changes while it is read.
//! - Alternatives: the client downloads and uploads (needs an upload call,
//!   sends the data over the person's link twice, out of reach of agents);
//!   the source pushes (upload calls with the target's policy per write);
//!   one mechanism for sessions and another for memory.
//! - Choice: the target pulls. The source snapshots the files into a
//!   transfer (copied, not linked: an append would change a link too) and
//!   answers a [`TransferManifest`] in the bundle's own shape (bundle path
//!   to hash and size); the target reads chunks by hash (`transfer.read`,
//!   like `fleet.blob`) and places them. Sessions and memory share it; the
//!   row that opens a transfer decides its scope (`session.export`
//!   operate, `memory.export` admin).
//! - Limits: [`MAX_TRANSFER_FILE`] and [`MAX_TRANSFER_TOTAL`], here and
//!   nowhere else, not configurable yet. A transfer over either is refused
//!   with `invalid_request` naming the limit and the size.
//! - Trade-off: a snapshot takes disk until it is closed or expires (10
//!   idle minutes, and every restart).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::fleet::{BlobHash, BundleFile, BundlePath};
use crate::harness::HarnessKind;
use crate::id::{HarnessSessionId, NodeId, SessionId, TransferId};
use crate::name::ProfileName;
use crate::session::SessionInfo;

/// The largest file a transfer carries: 256 MiB.
pub const MAX_TRANSFER_FILE: u64 = 256 << 20;

/// The most a transfer carries in all: 2 GiB.
pub const MAX_TRANSFER_TOTAL: u64 = 2 << 30;

/// `session.export` (protocol 6): snapshot a structured session's harness
/// files (never its profile's memory) into a transfer. A session mid-turn is
/// refused unless `force`, as for `session.move_profile`. An `operate` call
/// on that session, so an agent exports only what it started. Needs
/// `operate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExportSession {
    /// Which session.
    pub session: SessionId,
    /// Snapshot even while a turn runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// `memory.export` (protocol 6): snapshot a profile's memory, one
/// project's or all of it. Needs `admin`: memory is kept per account on
/// purpose (plan decision 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ExportMemory {
    /// Whose memory; absent is the harness's default profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileName>,
    /// One project's, by its working directory on this node; absent is
    /// every project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
}

/// Reply to `session.export` and `memory.export`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TransferManifest {
    /// The open transfer.
    pub transfer: TransferId,
    /// Every file, by its path under the harness home.
    pub files: BTreeMap<BundlePath, BundleFile>,
    /// What the files are.
    pub detail: TransferDetail,
}

/// What a transfer holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransferDetail {
    /// A session's harness files.
    Session {
        /// The harness's id for the conversation, to resume it by.
        harness_session: HarnessSessionId,
        /// The directory it ran in (the files are named after it).
        cwd: PathBuf,
        /// The session as its node reports it: harness, profile, creator.
        info: Box<SessionInfo>,
    },
    /// A profile's memory.
    Memory {
        /// Which harness.
        harness: HarnessKind,
        /// Whose memory; absent is the default profile.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<ProfileName>,
        /// The project it is for, if only one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<PathBuf>,
    },
}

/// `transfer.read` (protocol 6): part of one file of an open transfer, from
/// `offset`, as `fleet.blob` reads the bundle. Only the caller that opened
/// it may read it. Needs `read`. Answers a `BlobChunk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ReadTransfer {
    /// Which transfer.
    pub transfer: TransferId,
    /// Which file, by content.
    pub hash: BlobHash,
    /// Where to start, in bytes.
    #[serde(default)]
    pub offset: u64,
}

/// `transfer.close` (protocol 6): drop the snapshot. Only the caller that
/// opened it may close it. Needs `read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CloseTransfer {
    /// Which transfer.
    pub transfer: TransferId,
}

/// A session on a node of the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionRef {
    /// The node.
    pub node: NodeId,
    /// The session there.
    pub session: SessionId,
}

/// `session.import` (protocol 6): the target pulls a session from `from`
/// (exporting it there, through `fleet.forward` when it is another node),
/// places its files in the chosen profile's home, renamed for the working
/// directory here, and with `start` resumes it as a new session. A file
/// already there with other content refuses the import before anything is
/// written. Moving is importing, then killing the source. Needs `operate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ImportSession {
    /// The session to copy.
    pub from: SessionRef,
    /// The profile here; absent is the harness's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileName>,
    /// The working directory here; absent is the source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Start a session resuming the conversation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start: bool,
}

/// Reply to `session.import`: where the conversation is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SessionImported {
    /// The harness's id for it, to resume it by later.
    pub harness_session: HarnessSessionId,
    /// The profile it was placed in; absent is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileName>,
    /// The working directory it belongs to here.
    pub cwd: PathBuf,
    /// Files written.
    pub written: u32,
    /// The new session, when `start` asked for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Box<SessionInfo>>,
}

/// A profile's memory on a node of the fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct MemoryRef {
    /// The node.
    pub node: NodeId,
    /// Whose memory there; absent is the default profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileName>,
    /// One project's, by its working directory there; absent is all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
}

/// `memory.import` (protocol 6): the same pull for memory. A file that
/// differs from one already there is written beside it
/// (`.vessel-incoming`), as the fleet's materialiser does; nothing is
/// overwritten. Needs `admin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ImportMemory {
    /// The memory to copy.
    pub from: MemoryRef,
    /// The profile here; absent is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileName>,
    /// The project here, when copying one; absent is the source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
}

/// Reply to `memory.import`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct MemoryImported {
    /// Files written.
    pub written: u32,
    /// Files already there with the same content.
    pub same: u32,
    /// Files that differed, written beside the ones there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub beside: Vec<BundlePath>,
}
