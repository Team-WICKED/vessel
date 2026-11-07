//! Errors of this crate.

use std::path::PathBuf;
use std::sync::Arc;

use vessel_proto::{HarnessKind, ProfileName};

use crate::mcp::McpError;

/// A session could not be turned into a command. Not `PartialEq`: it keeps
/// I/O errors as their source.
#[derive(Debug, Clone, thiserror::Error)]
pub enum HarnessError {
    /// The harness binary is not on the search path.
    #[error("{harness} is not installed: `{binary}` was not found")]
    NotInstalled {
        /// Which harness.
        harness: HarnessKind,
        /// The binary that was looked for.
        binary: &'static str,
    },
    /// Structured mode cannot run on this node (for example its ACP adapter
    /// is missing).
    #[error("{harness} cannot run in structured mode: {reason}")]
    StructuredUnavailable {
        /// Which harness.
        harness: HarnessKind,
        /// What is missing and how to fix it.
        reason: String,
    },
    /// The requested working directory is relative. It would be resolved
    /// against the node's own working directory, which the client does not
    /// know, so it is refused; clients send absolute paths.
    #[error(
        "working directory {} is relative; send an absolute path (the node's own working directory means nothing to the client)",
        cwd.display()
    )]
    RelativeDirectory {
        /// The rejected directory.
        cwd: PathBuf,
    },
    /// The requested working directory is not a directory.
    #[error("working directory {} does not exist or is not a directory", cwd.display())]
    NoSuchDirectory {
        /// The rejected directory.
        cwd: PathBuf,
    },
    /// The profile extends another harness than the session's.
    #[error("profile {profile} is for {profile_harness}, not {harness}")]
    ProfileMismatch {
        /// The profile.
        profile: ProfileName,
        /// The harness it extends.
        profile_harness: HarnessKind,
        /// The session's harness.
        harness: HarnessKind,
    },
    /// The profile is not the one the spec asks for (`spec.profile`, or the
    /// harness's default when it names none). The caller resolves the name;
    /// launching with another profile would run the session as another
    /// account than recorded.
    #[error("the session asks for profile {}, not {given}", requested.as_ref().map_or("(the default)", ProfileName::as_str))]
    ProfileNotRequested {
        /// What the spec names.
        requested: Option<ProfileName>,
        /// The profile given.
        given: ProfileName,
    },
    /// The MCP servers cannot be given to this harness's terminal session.
    #[error(transparent)]
    Mcp(#[from] McpError),
    /// The harness reads its MCP servers from a file and no owner-only
    /// directory was given for it (there is no shared fallback).
    #[error("no directory for the session's MCP configuration file")]
    NoMcpDirectory,
    /// No name could be chosen for the session's MCP configuration file
    /// (the system's random source failed).
    #[error("cannot name an MCP configuration file")]
    McpFile(#[source] Arc<std::io::Error>),
    /// A profile with a model route came without the session's proxy, or
    /// a proxy with a profile that has none (ADR 0015): the caller resolves
    /// the route, and launching the harness against its own vendor instead
    /// would be a silent change of model.
    #[error("profile {profile}: its model route and the session's proxy do not go together")]
    Unrouted {
        /// The profile.
        profile: ProfileName,
    },
    /// A routed session's own arguments (its profile's or its spec's) would
    /// choose another model, provider or configuration profile (the
    /// security review of milestone 5, finding 6).
    #[error("`{argument}` would take this routed session off its model route")]
    RoutedArgument {
        /// The argument.
        argument: String,
    },
}
