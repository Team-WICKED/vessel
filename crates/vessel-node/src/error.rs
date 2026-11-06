//! Errors: why a node could not start ([`NodeError`]), and why a call
//! failed (`ServiceError`, mapped to the wire in one place).

use std::net::SocketAddr;

use vessel_proto::{ErrorCode, EventSeq, HarnessKind, ProfileName, SessionId, WireError};
use vessel_rpc::OutboundError;
use vessel_session::SessionError;
use vessel_store::StoreError;

use crate::persist::Unpersisted;
use crate::profiles::ProfilesFileError;

/// An error and its causes on one line, for a log: a device connection
/// that "ended abnormally" with "transport failed" says nothing until its
/// causes (the channel's, the socket's) follow.
pub(crate) fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// A node failed to start, or its server failed.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The data directory, node id or token could not be prepared.
    #[error("the data directory is unusable")]
    DataDir(#[source] StoreError),
    /// The event log could not be opened.
    #[error("the event log could not be opened")]
    Store(#[source] StoreError),
    /// Another node already uses this data directory.
    #[error("another node already uses this data directory")]
    AlreadyRunning(#[source] StoreError),
    /// Relays are configured without `--relay-token-file`.
    #[error("a relay needs its registration token: pass --relay-token-file")]
    NoRelayToken,
    /// The relays' registration token file is unreadable or empty.
    #[error(transparent)]
    RelayToken(#[from] vessel_relay::TokenFileError),
    /// A `wss://` relay is configured, and no TLS root certificate could
    /// be loaded to verify it.
    #[error("cannot verify wss:// relays")]
    RelayTls(#[source] vessel_relay::TlsError),
    /// The local socket (or named pipe) could not be bound.
    #[error("cannot listen on the local socket {}", path.display())]
    LocalSocket {
        /// The socket path or pipe name.
        path: std::path::PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The listen address could not be bound.
    #[error("cannot listen on {address}")]
    Bind {
        /// The address.
        address: SocketAddr,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// `profiles.toml` cannot be read or does not parse; the node does not
    /// start rather than run sessions under a guess (ADR 0011).
    #[error("profiles.toml cannot be used")]
    Profiles(#[source] ProfilesFileError),
    /// The data directory's MCP directory cannot be prepared.
    #[error("the MCP configuration directory is unusable")]
    McpDir(#[source] StoreError),
    /// The HTTP server failed.
    #[error("the server failed")]
    Serve(#[source] std::io::Error),
    /// A startup or shutdown task failed unexpectedly.
    #[error("a node task failed")]
    Task(#[source] tokio::task::JoinError),
}

/// Why a call failed inside the node. Mapped to the wire once, by
/// [`ServiceError::to_wire`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServiceError {
    /// No session with this id, live or stored.
    #[error("no session {0}")]
    NotFound(SessionId),
    /// The session has ended and is served from the store (it ended in an
    /// earlier run, or in this one and was handed over); it can be read but
    /// not driven.
    #[error("session {0} has ended; its log can still be read")]
    Ended(SessionId),
    /// `from` is past the end of the session's log.
    #[error("session {session} has {next} records; cannot start at {from}")]
    Ahead {
        /// The session.
        session: SessionId,
        /// Requested start.
        from: EventSeq,
        /// The log's length.
        next: EventSeq,
    },
    /// The node is draining for an update and starts no new sessions.
    #[error("the node is draining for an update and starts no new sessions")]
    Draining,
    /// The session runtime refused or failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The event log failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The session's records could not all be persisted.
    #[error(transparent)]
    Unpersisted(#[from] Unpersisted),
    /// The connection cannot take the subscription.
    #[error(transparent)]
    Outbound(#[from] OutboundError),
    /// A task the call depended on failed.
    #[error("a node task failed")]
    Task(#[source] tokio::task::JoinError),
    /// `device.invite` with a lifetime of zero.
    #[error("an invitation must be valid for at least one second")]
    InvalidTtl,
    /// `device.invite` naming an empty list of scopes.
    #[error("an invitation must grant at least one scope")]
    NoScopes,
    /// Too many invitations are waiting (`devices::invitations`).
    #[error("too many pairing invitations are waiting; let some be used or expire")]
    TooManyInvitations,
    /// The operating system's random source failed.
    #[error("no randomness available")]
    Random(#[source] getrandom::Error),
    /// The pairing link could not be encoded.
    #[error("could not encode the pairing link")]
    Link(#[source] serde_json::Error),
    /// `spec.profile` names no profile on this node.
    #[error("this node has no profile `{0}` (see `profile.list`)")]
    UnknownProfile(ProfileName),
    /// `spec.profile` names a profile of another harness.
    #[error("profile `{profile}` is for {profile_harness}, not {harness}")]
    ProfileHarness {
        /// The profile.
        profile: ProfileName,
        /// The harness it extends.
        profile_harness: HarnessKind,
        /// The session's harness.
        harness: HarnessKind,
    },
    /// A session runs under a profile `profiles.toml` no longer defines, so
    /// its files cannot be found to move.
    #[error("the session's profile `{0}` is no longer defined")]
    ProfileGone(ProfileName),
    /// A profile's harness home is a relative path, which the harness
    /// resolves per session.
    #[error("profile `{0}`'s harness home cannot be known (a relative path)")]
    HomeUnknown(ProfileName),
    /// An agent's session is gone while it asks for a new one.
    #[error("the calling agent's session {0} has ended")]
    AgentGone(SessionId),
    /// Agents may start agents only so deep (ADR 0013).
    #[error("agents may start sessions at most {limit} levels deep")]
    AgentDepth {
        /// The limit.
        limit: u8,
    },
    /// An agent owns as many live sessions as it may (ADR 0013).
    #[error("an agent may own at most {limit} live sessions; end one first")]
    AgentLimit {
        /// The limit.
        limit: usize,
    },
    /// An agent asked to operate on a session it did not start (ADR 0013).
    #[error("an agent may only operate on sessions it started, or that those started")]
    NotOwned(SessionId),
    /// The `vessel` server's definition cannot be built (a name the
    /// protocol refuses: a bug).
    #[error("cannot define the `vessel` MCP server")]
    AgentTools(#[source] vessel_proto::InvalidName),
    /// The node's local endpoint is not Unicode, so `vessel mcp` cannot be
    /// told where it is.
    #[error("the node's local socket path is not Unicode")]
    AgentToolsPath,
    /// A profile move of this session is running already.
    #[error("session {0} is already moving to another profile")]
    MoveRunning(SessionId),
    /// The target profile's account is not logged in (its harness's check
    /// failed, or did not answer in time).
    #[error("profile `{0}` is not logged in; log it in first")]
    NotLoggedIn(ProfileName),
    /// `profile.reload` found a file it cannot use; the profiles in effect
    /// stay.
    #[error("the profiles in effect stay")]
    Profiles(#[source] ProfilesFileError),
    /// A transfer could not be opened, read, pulled or placed (protocol
    /// 6).
    #[error(transparent)]
    Transfer(#[from] crate::transfers::TransferError),
}

impl ServiceError {
    /// The error as the client sees it: a code to branch on and the full
    /// chain of causes as text (none of which contains secrets).
    pub(crate) fn to_wire(&self) -> WireError {
        let code = match self {
            Self::NotFound(_) => ErrorCode::NotFound,
            // `Profiles(Parse)`: the call is fine; the node's file is not
            // (and stays unused).
            Self::Ended(_)
            | Self::Profiles(ProfilesFileError::Parse(_))
            | Self::ProfileGone(_)
            | Self::HomeUnknown(_)
            | Self::NotLoggedIn(_)
            | Self::AgentGone(_)
            | Self::MoveRunning(_) => ErrorCode::Conflict,
            Self::AgentDepth { .. } | Self::AgentLimit { .. } | Self::NotOwned(_) => {
                ErrorCode::Forbidden
            }
            Self::Draining | Self::TooManyInvitations => ErrorCode::Unavailable,
            Self::Ahead { .. }
            | Self::InvalidTtl
            | Self::NoScopes
            | Self::UnknownProfile(_)
            | Self::ProfileHarness { .. } => ErrorCode::InvalidRequest,
            Self::Session(error) => error.code(),
            Self::Transfer(error) => error.code(),
            Self::Store(error) | Self::Profiles(ProfilesFileError::Read(error)) => error.code(),
            Self::Outbound(error) => error.code(),
            Self::Unpersisted(_)
            | Self::Task(_)
            | Self::Random(_)
            | Self::Link(_)
            | Self::AgentTools(_)
            | Self::AgentToolsPath => ErrorCode::Internal,
        };
        let mut message = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        WireError::new(code, message)
    }
}
