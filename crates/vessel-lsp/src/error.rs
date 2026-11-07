//! Failures retain their source inside the runtime; the node maps them once.

/// Language-server runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// No configured executable was found.
    #[error("language server is not installed or configured")]
    NotInstalled,
    /// Conservative analysis refuses executable or unsupported project settings.
    #[error(
        "project configuration can run code; the operator can enable full analysis for this root"
    )]
    ProjectConfiguration,
    /// Conservative Rust analysis links only checked Cargo projects, and
    /// the root has none: the server is never left to discover one itself.
    #[error("no Cargo project found for this workspace; Rust analysis is unavailable")]
    NoProject,
    /// Conservative Python analysis on Windows needs an operator-pinned
    /// interpreter: a bare `python` lookup can select a repository binary.
    #[error(
        "Python analysis on Windows needs an operator-configured python_interpreter; it is unavailable"
    )]
    PythonInterpreter,
    /// Only trusted roots and regular saved files may be read.
    #[error("path is outside the authorized workspace or is not a regular saved file")]
    Path,
    /// A request exceeds a fixed resource budget.
    #[error("language-server resource budget exceeded")]
    Limit,
    /// A capability was not advertised.
    #[error("language server does not support this query")]
    Unsupported,
    /// Node shutdown, queue saturation or restart backoff.
    #[error("language server is unavailable; retry later")]
    Unavailable,
    /// A server deadline expired.
    #[error("language server did not answer within its deadline")]
    Timeout,
    /// The server closed a pipe or gave an unexpected correlation id.
    #[error("language server disconnected or violated the protocol")]
    Protocol,
    /// The server reported a failure. Its arbitrary message is not exposed.
    #[error("language server rejected the request (code {0})")]
    Server(i32),
    /// Saved filesystem or pipe failure.
    #[error("language-server I/O failed")]
    Io(#[from] std::io::Error),
    /// Bounded LSP transport failure.
    #[error(transparent)]
    Frame(#[from] crate::framing::FrameError),
    /// JSON mismatched the typed subset.
    #[error("language-server message did not match the expected type")]
    Json(#[from] serde_json::Error),
    /// Process startup failed.
    #[error("cannot start language server")]
    Spawn(#[from] vessel_proc::SpawnError),
    /// Process exit could not be observed.
    #[error("cannot reap language server")]
    Wait(#[from] vessel_proc::WaitError),
    /// Saved source position is invalid.
    #[error(transparent)]
    Position(#[from] crate::position::PositionError),
    /// Source text is not UTF-8.
    #[error("saved source is not UTF-8")]
    Encoding(#[from] std::string::FromUtf8Error),
    /// A native watcher failed.
    #[error("cannot watch saved source changes")]
    Watch(#[from] notify::Error),
    /// A blocking filesystem task could not finish.
    #[error("saved-file task could not finish")]
    Task(#[from] tokio::task::JoinError),
    /// URI could not be parsed.
    #[error("invalid server file URI")]
    Uri(#[from] url::ParseError),
}
