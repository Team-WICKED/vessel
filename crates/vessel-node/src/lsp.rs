//! Authorize source roots at the destination before any LSP process starts.
//!
//! Delegation never inherits a source node's cwd. A local agent's cwd is an
//! implicit read root, but full analysis always requires an operator entry.

use std::path::PathBuf;

use vessel_lsp::{LspError as Error, Query};
use vessel_proto::{ErrorCode, GetSession, Request, WireError};

use crate::principal::Principal;
use crate::service::{Connection, NodeService};

/// An operator-approved destination root, never configured by an RPC request.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspRoot {
    /// Absolute directory available to Read callers, including fleet calls.
    pub path: PathBuf,
    /// Permit repository build scripts/proc macros for more accurate analysis.
    /// False is the conservative default for every implicit session root.
    #[serde(default)]
    pub full_analysis: bool,
}

impl NodeService {
    async fn lsp_query<R: Request>(
        &self,
        connection: &Connection,
        query: Query,
    ) -> Result<R::Response, WireError> {
        let requested = &query.workspace().root;
        // Refuse lexical escape and network syntax before filesystem access.
        if !requested.is_absolute()
            || requested
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(WireError::new(
                ErrorCode::InvalidRequest,
                "LSP root must be absolute without parent traversal",
            ));
        }
        let mut allowed = self.lsp_roots.clone();
        if let Principal::Agent { session, .. } = connection.principal() {
            let info = self
                .get_session(connection, GetSession { session: *session })
                .await?;
            if let Some(cwd) = info.spec.cwd {
                allowed.push(LspRoot {
                    path: cwd,
                    full_analysis: false,
                });
            }
        }
        // The lexical check avoids touching an unrelated path just to deny
        // it. Canonical checks then reject symlinks under an allowed path.
        if !allowed
            .iter()
            .any(|entry| requested.starts_with(&entry.path))
        {
            return Err(WireError::new(
                ErrorCode::Forbidden,
                "LSP workspace is not an allowed root on this node",
            ));
        }
        let root = vessel_lsp::documents::canonical_root(requested)
            .await
            .map_err(|error| wire_error(&error))?;
        let mut permitted = false;
        let mut full = false;
        for entry in allowed {
            let Ok(canonical) = vessel_lsp::documents::canonical_root(&entry.path).await else {
                continue;
            };
            if root.starts_with(canonical) {
                permitted = true;
                full |= entry.full_analysis;
            }
        }
        if !permitted {
            // The same answer as a missing path: a symlink inside an allowed
            // root must not reveal whether its outside target exists.
            return Err(wire_error(&Error::Path));
        }
        let reply = self
            .lsp
            .query(root, full, query)
            .await
            .map_err(|error| wire_error(&error))?;
        R::response_from(reply)
            .map_err(|_mismatch| WireError::new(ErrorCode::Internal, "LSP reply type mismatch"))
    }
}

macro_rules! handlers {
    ($( $name:ident, $request:ident, $variant:ident; )*) => {$(
        impl NodeService {
            pub(crate) async fn $name(&self, connection: &Connection, request: vessel_proto::lsp::$request) -> Result<<vessel_proto::lsp::$request as Request>::Response, WireError> {
                self.lsp_query::<vessel_proto::lsp::$request>(connection, Query::$variant(request)).await
            }
        }
    )*};
}
handlers! {
    lsp_workspace_symbols, WorkspaceSymbols, WorkspaceSymbols;
    lsp_document_symbols, DocumentSymbols, DocumentSymbols;
    lsp_definition, Definition, Definition;
    lsp_references, References, References;
    lsp_hover, GetHover, Hover;
    lsp_implementations, Implementations, Implementations;
    lsp_type_definition, TypeDefinition, TypeDefinition;
    lsp_incoming_calls, IncomingCalls, IncomingCalls;
    lsp_outgoing_calls, OutgoingCalls, OutgoingCalls;
    lsp_diagnostics, Diagnostics, Diagnostics;
}

fn wire_error(error: &Error) -> WireError {
    let code = match error {
        Error::NotInstalled | Error::ProjectConfiguration | Error::Unavailable | Error::Timeout => {
            ErrorCode::Unavailable
        }
        Error::Path => ErrorCode::Forbidden,
        Error::Limit | Error::Position(_) | Error::Encoding(_) => ErrorCode::InvalidRequest,
        Error::Unsupported => ErrorCode::Unsupported,
        Error::Spawn(_) => ErrorCode::SpawnFailed,
        Error::Wait(_)
        | Error::Protocol
        | Error::Server(_)
        | Error::Io(_)
        | Error::Frame(_)
        | Error::Json(_)
        | Error::Watch(_)
        | Error::Task(_)
        | Error::Uri(_) => ErrorCode::Internal,
    };
    // Display is deliberately generic: raw server errors, stdout, filenames
    // from rejected results and arbitrary stderr never cross this boundary.
    WireError::new(code, error.to_string())
}
