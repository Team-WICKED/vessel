//! Typed requests in the closed read-only LSP contract.

use super::{FilePath, PageRequest, Position, Workspace};
use serde::{Deserialize, Serialize};

/// Workspace symbol search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSymbols {
    /// Authorized server selection.
    pub workspace: Workspace,
    /// Search text; bounded by the node.
    pub query: String,
    /// Sorted result page.
    #[serde(default)]
    pub page: PageRequest,
}

/// Symbols in one saved file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSymbols {
    /// Authorized server selection.
    pub workspace: Workspace,
    /// Relative saved file.
    pub file: FilePath,
    /// Sorted result page.
    #[serde(default)]
    pub page: PageRequest,
}

/// Shared fields for a query at a saved source position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtPosition {
    /// Authorized server selection.
    pub workspace: Workspace,
    /// Relative saved file.
    pub file: FilePath,
    /// One-based Unicode scalar position.
    pub position: Position,
    /// Sorted result page (unused by hover).
    #[serde(default)]
    pub page: PageRequest,
}

macro_rules! position_request {
    ($( $(#[$doc:meta])* $name:ident; )*) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub AtPosition);
    )*};
}
position_request! {
    /// Definitions at a saved source position.
    Definition;
    /// Hover at a saved source position.
    GetHover;
    /// Implementations at a saved source position.
    Implementations;
    /// Type definitions at a saved source position.
    TypeDefinition;
    /// Calls to the symbol at a saved source position.
    IncomingCalls;
    /// Calls from the symbol at a saved source position.
    OutgoingCalls;
}

/// References at a source position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct References {
    /// Source position and page.
    #[serde(flatten)]
    pub at: AtPosition,
    /// Whether declaration locations should also be included.
    #[serde(default)]
    pub include_declaration: bool,
}

/// File diagnostics, or the known workspace diagnostic cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostics {
    /// Authorized server selection.
    pub workspace: Workspace,
    /// One file, or all files the server has reported.
    pub file: Option<FilePath>,
    /// Sorted result page.
    #[serde(default)]
    pub page: PageRequest,
}
