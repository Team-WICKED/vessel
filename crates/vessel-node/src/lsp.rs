//! Protocol 7 LSP dispatch seam; implementation follows the typed contract.

use crate::service::{Connection, NodeService};
use vessel_proto::{ErrorCode, WireError};

#[expect(clippy::unused_async, reason = "the staged contract keeps the asynchronous handler signature before the runtime lands")]
impl NodeService {
    pub(crate) async fn lsp_workspace_symbols(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::WorkspaceSymbols,
    ) -> Result<vessel_proto::lsp::Page<vessel_proto::lsp::Symbol>, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
    pub(crate) async fn lsp_document_symbols(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::DocumentSymbols,
    ) -> Result<vessel_proto::lsp::Page<vessel_proto::lsp::Symbol>, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
    pub(crate) async fn lsp_definition(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::Definition,
    ) -> Result<vessel_proto::lsp::Page<vessel_proto::lsp::Location>, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
    pub(crate) async fn lsp_references(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::References,
    ) -> Result<vessel_proto::lsp::Page<vessel_proto::lsp::Location>, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
    pub(crate) async fn lsp_hover(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::GetHover,
    ) -> Result<vessel_proto::lsp::Hover, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
    pub(crate) async fn lsp_implementations(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::Implementations,
    ) -> Result<vessel_proto::lsp::Page<vessel_proto::lsp::Location>, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
    pub(crate) async fn lsp_type_definition(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::TypeDefinition,
    ) -> Result<vessel_proto::lsp::Page<vessel_proto::lsp::Location>, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
    pub(crate) async fn lsp_incoming_calls(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::IncomingCalls,
    ) -> Result<vessel_proto::lsp::Page<vessel_proto::lsp::Call>, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
    pub(crate) async fn lsp_outgoing_calls(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::OutgoingCalls,
    ) -> Result<vessel_proto::lsp::Page<vessel_proto::lsp::Call>, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
    pub(crate) async fn lsp_diagnostics(
        &self,
        _connection: &Connection,
        _request: vessel_proto::lsp::Diagnostics,
    ) -> Result<vessel_proto::lsp::Page<vessel_proto::lsp::Diagnostic>, WireError> {
        Err(WireError::new(
            ErrorCode::Unsupported,
            "LSP context is not implemented on this node yet",
        ))
    }
}
