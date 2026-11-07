//! Saved-file IDE context (protocol 7, ADR 0016).
//!
//! The wire exposes a closed read-only subset, never arbitrary server JSON.
//! Paths are lexically validated here and canonically authorized by the node.
//! Positions count Unicode scalar values from one; LSP UTF-16 stays private.

use std::num::NonZeroU32;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod requests;
pub use requests::{
    AtPosition, Definition, Diagnostics, DocumentSymbols, GetHover, Implementations, IncomingCalls,
    OutgoingCalls, References, TypeDefinition, WorkspaceSymbols,
};

/// A supported server family; JavaScript uses the TypeScript server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// rust-analyzer.
    Rust,
    /// typescript-language-server.
    TypeScript,
    /// basedpyright-langserver or pyright-langserver.
    Python,
    /// gopls.
    Go,
    /// clangd, for C and C++.
    Cpp,
}

/// A lexical path error. Filesystem authority is checked by the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidFilePath;

impl std::fmt::Display for InvalidFilePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("expected a nonempty relative file path without traversal or network syntax")
    }
}
impl std::error::Error for InvalidFilePath {}

/// A file under the authorized workspace, never an absolute path or URI.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
// Serialized through `PathBuf`: a string on the wire, which ts-rs cannot
// see through `try_from`/`into`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
#[serde(try_from = "PathBuf", into = "PathBuf")]
pub struct FilePath(PathBuf);

impl TryFrom<PathBuf> for FilePath {
    type Error = InvalidFilePath;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        let Some(text) = path.to_str() else {
            return Err(InvalidFilePath);
        };
        if text.is_empty()
            || text.len() > 4096
            || text.chars().any(char::is_control)
            || text.contains(['\\', ':', '\0'])
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(InvalidFilePath);
        }
        Ok(Self(path))
    }
}

impl From<FilePath> for PathBuf {
    fn from(path: FilePath) -> Self {
        path.0
    }
}

impl FilePath {
    /// The relative path, to resolve only under an authorized workspace.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// The server identity a query selects. This is not a filesystem grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Workspace {
    /// Absolute root on the destination node; the node checks authority.
    pub root: PathBuf,
    /// One server family per canonical workspace root.
    pub language: Language,
}

/// A 1-based Unicode scalar position; the line's exclusive end is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Position {
    /// One-based line.
    pub line: NonZeroU32,
    /// One-based Unicode scalar column, not a byte or UTF-16 offset.
    pub column: NonZeroU32,
}

/// An exclusive-end source range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Range {
    /// Inclusive start.
    pub start: Position,
    /// Exclusive end.
    pub end: Position,
}

/// A location relative to the authorized workspace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Location {
    /// Saved source file.
    pub file: FilePath,
    /// Scalar range.
    pub range: Range,
}

/// A best-effort offset into sorted results; edits may shift later pages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(transparent)]
pub struct Offset(pub u32);

/// The requested page. Limits are capped at 200 by the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PageRequest {
    /// Zero-based result offset.
    #[serde(default)]
    pub offset: Offset,
    /// Items requested (default 50, capped at 200).
    pub limit: NonZeroU32,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            offset: Offset::default(),
            limit: NonZeroU32::MIN.saturating_add(49),
        }
    }
}

/// What progress the server actually reported, never inferred from silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Indexing {
    /// No completion signal has been observed.
    Unknown,
    /// One or more work-done tokens remain open.
    Busy,
    /// Observed work-done tokens ended; not a promise about unseen work.
    ObservedIdle,
}

/// How saved contents relate to the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// Query ran against the saved text synchronized before the request.
    Saved,
    /// Diagnostics came from an earlier notification; versions may be absent.
    Cached,
    /// No report is available for the requested scope.
    Unknown,
}

/// Scope and age of every answer, including empty answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Context {
    /// Files represented by this result/cache, never a promise of all files.
    pub files: Vec<FilePath>,
    /// Query observation time, or the oldest contributing cached report;
    /// absent when nothing was reported.
    pub observed_at: Option<DateTime<Utc>>,
    /// Progress actually observed on this server generation.
    pub indexing: Indexing,
    /// Saved query, cached report, or unknown.
    pub freshness: Freshness,
    /// True only for a server report explicitly covering the whole workspace.
    pub workspace_complete: bool,
}

/// A bounded page, deterministically sorted before offset selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Page<T> {
    /// At most the requested number of items, capped by the node.
    pub items: Vec<T>,
    /// Next offset within the collected result, if more remain.
    pub next: Option<Offset>,
    /// True if item/byte budgets cut off some server data.
    pub truncated: bool,
    /// Coverage, freshness and observed indexing progress.
    pub context: Context,
}

/// LSP symbol kinds, including a typed unknown for future protocol values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// File.
    File,
    /// Module.
    Module,
    /// Namespace.
    Namespace,
    /// Package.
    Package,
    /// Class.
    Class,
    /// Method.
    Method,
    /// Property.
    Property,
    /// Field.
    Field,
    /// Constructor.
    Constructor,
    /// Enum.
    Enum,
    /// Interface.
    Interface,
    /// Function.
    Function,
    /// Variable.
    Variable,
    /// Constant.
    Constant,
    /// String.
    String,
    /// Number.
    Number,
    /// Boolean.
    Boolean,
    /// Array.
    Array,
    /// Object.
    Object,
    /// Key.
    Key,
    /// Null.
    Null,
    /// Enum member.
    EnumMember,
    /// Struct.
    Struct,
    /// Event.
    Event,
    /// Operator.
    Operator,
    /// Type parameter.
    TypeParameter,
    /// A server value outside the known LSP set.
    Unknown,
}

/// A symbol in saved source. Nested document symbols are flattened.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Symbol {
    /// Bounded display name.
    pub name: String,
    /// Its semantic kind.
    pub kind: SymbolKind,
    /// Name/selection location.
    pub location: Location,
    /// Optional enclosing symbol name.
    pub container: Option<String>,
}

/// Diagnostic severity; absence is distinct from an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Error.
    Error,
    /// Warning.
    Warning,
    /// Information.
    Information,
    /// Hint.
    Hint,
    /// Missing or unrecognized severity.
    Unknown,
}

/// A bounded diagnostic with only authorized source locations.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Diagnostic {
    /// Diagnostic location.
    pub location: Location,
    /// Server severity.
    pub severity: Severity,
    /// Bounded explanatory text.
    pub message: String,
    /// Optional bounded server/source label.
    pub source: Option<String>,
}

/// A call edge anchored at its caller's source sites.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
// `LspCall` in TypeScript: one file declares every wire type, and `Call`
// is the RPC call there.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "LspCall"))]
pub struct Call {
    /// Caller for incoming calls, callee for outgoing calls.
    pub symbol: Symbol,
    /// Authorized call sites, bounded along with all other locations.
    pub sites: Vec<Location>,
}

/// Hover text is untrusted content; clients must not render raw HTML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Hover {
    /// Plain text or Markdown as indicated by `format`.
    pub text: String,
    /// The text's representation.
    pub format: Markup,
    /// Optional range in the requested file.
    pub range: Option<Range>,
    /// Whether text was shortened to fit its budget.
    pub truncated: bool,
    /// Scope and freshness, even when text is empty.
    pub context: Context,
}

/// How a bounded hover text is represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Markup {
    /// Literal text.
    PlainText,
    /// Untrusted Markdown, with HTML disabled by the consumer.
    Markdown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Call as RpcCall, Request, Scope};

    #[test]
    fn invalid_paths_and_zero_positions_fail_at_the_edge() {
        for path in [
            "",
            "../secret",
            "/etc/passwd",
            "a/../b",
            "C:\\file",
            "a\\b",
            "file:///x",
        ] {
            assert!(FilePath::try_from(PathBuf::from(path)).is_err(), "{path}");
        }
        for json in [r#"{"line":0,"column":1}"#, r#"{"line":1,"column":0}"#] {
            assert!(serde_json::from_str::<Position>(json).is_err());
        }
    }

    #[test]
    fn every_lsp_method_roundtrips_with_read_scope_and_version_seven() {
        let workspace = Workspace {
            root: PathBuf::from("/work"),
            language: Language::Rust,
        };
        let file = FilePath::try_from(PathBuf::from("src/lib.rs")).unwrap();
        let at = AtPosition {
            workspace: workspace.clone(),
            file: file.clone(),
            position: Position {
                line: NonZeroU32::MIN,
                column: NonZeroU32::MIN,
            },
            page: PageRequest::default(),
        };
        let calls = [
            WorkspaceSymbols {
                workspace: workspace.clone(),
                query: "name".into(),
                page: PageRequest::default(),
            }
            .into_call(),
            DocumentSymbols {
                workspace: workspace.clone(),
                file,
                page: PageRequest::default(),
            }
            .into_call(),
            Definition(at.clone()).into_call(),
            References {
                at: at.clone(),
                include_declaration: true,
            }
            .into_call(),
            GetHover(at.clone()).into_call(),
            Implementations(at.clone()).into_call(),
            TypeDefinition(at.clone()).into_call(),
            IncomingCalls(at.clone()).into_call(),
            OutgoingCalls(at).into_call(),
            Diagnostics {
                workspace,
                file: None,
                page: PageRequest::default(),
            }
            .into_call(),
        ];
        for call in calls {
            assert_eq!(call.method().scope(), Scope::Read);
            assert_eq!(call.since().get(), 7);
            assert!(call.method().forwardable());
            let json = serde_json::to_string(&call).unwrap();
            assert_eq!(serde_json::from_str::<RpcCall>(&json).unwrap(), call);
        }
    }
}
