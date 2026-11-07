//! The closed read-only query set and bounded agent-facing conversion.

use vessel_proto::{Reply, lsp};

use crate::error::LspError as Error;
use crate::process::Process;
use crate::sync::Watcher;

mod convert;
mod diagnostics;
mod features;

pub(crate) use convert::truncate;

/// The only language-server queries the node can request.
#[derive(Debug, Clone)]
pub enum Query {
    /// Find workspace symbols.
    WorkspaceSymbols(lsp::WorkspaceSymbols),
    /// List a saved file's symbols.
    DocumentSymbols(lsp::DocumentSymbols),
    /// Resolve definitions.
    Definition(lsp::Definition),
    /// Resolve references.
    References(lsp::References),
    /// Read hover text.
    Hover(lsp::GetHover),
    /// Resolve implementations.
    Implementations(lsp::Implementations),
    /// Resolve type definitions.
    TypeDefinition(lsp::TypeDefinition),
    /// Calls to this symbol.
    IncomingCalls(lsp::IncomingCalls),
    /// Calls from this symbol.
    OutgoingCalls(lsp::OutgoingCalls),
    /// Known saved-file diagnostics.
    Diagnostics(lsp::Diagnostics),
}

impl Query {
    pub(crate) fn file(&self) -> Option<&lsp::FilePath> {
        match self {
            Self::WorkspaceSymbols(_) => None,
            Self::DocumentSymbols(request) => Some(&request.file),
            Self::References(request) => Some(&request.at.file),
            Self::Definition(lsp::Definition(at))
            | Self::Hover(lsp::GetHover(at))
            | Self::Implementations(lsp::Implementations(at))
            | Self::TypeDefinition(lsp::TypeDefinition(at))
            | Self::IncomingCalls(lsp::IncomingCalls(at))
            | Self::OutgoingCalls(lsp::OutgoingCalls(at)) => Some(&at.file),
            Self::Diagnostics(request) => request.file.as_ref(),
        }
    }

    /// The requested root/language; the node authorizes it before execution.
    #[must_use]
    pub const fn workspace(&self) -> &lsp::Workspace {
        match self {
            Self::WorkspaceSymbols(request) => &request.workspace,
            Self::DocumentSymbols(request) => &request.workspace,
            Self::References(request) => &request.at.workspace,
            Self::Definition(lsp::Definition(at))
            | Self::Hover(lsp::GetHover(at))
            | Self::Implementations(lsp::Implementations(at))
            | Self::TypeDefinition(lsp::TypeDefinition(at))
            | Self::IncomingCalls(lsp::IncomingCalls(at))
            | Self::OutgoingCalls(lsp::OutgoingCalls(at)) => &at.workspace,
            Self::Diagnostics(request) => &request.workspace,
        }
    }
}

impl Process {
    pub(crate) async fn query(&mut self, query: Query, watcher: &Watcher) -> Result<Reply, Error> {
        // Servers cancel reads while their own index changes. Retry only
        // after observing fresh progress, within the whole-query deadline.
        for _attempt in 0..3 {
            match self.query_once(query.clone(), watcher).await {
                Err(Error::Server(-32801 | -32802)) => self.ready().await?,
                outcome => return outcome,
            }
        }
        Err(Error::Unavailable)
    }

    async fn query_once(&mut self, query: Query, watcher: &Watcher) -> Result<Reply, Error> {
        // A native watcher can coalesce events or lose them. Rechecking the
        // bounded open set before each query preserves saved-file semantics.
        self.refresh_all(watcher).await?;
        self.ready().await?;
        Ok(match query {
            Query::WorkspaceSymbols(request) => {
                Reply::LspWorkspaceSymbols(self.workspace_symbols(request).await?)
            }
            Query::DocumentSymbols(request) => {
                Reply::LspDocumentSymbols(self.document_symbols(request, watcher).await?)
            }
            Query::Definition(lsp::Definition(at)) => {
                Reply::LspDefinition(self.locations(at, Feature::Definition, watcher).await?)
            }
            Query::References(request) => {
                Reply::LspReferences(self.references(request, watcher).await?)
            }
            Query::Hover(lsp::GetHover(at)) => Reply::LspHover(self.hover(at, watcher).await?),
            Query::Implementations(lsp::Implementations(at)) => Reply::LspImplementations(
                self.locations(at, Feature::Implementation, watcher).await?,
            ),
            Query::TypeDefinition(lsp::TypeDefinition(at)) => Reply::LspTypeDefinition(
                self.locations(at, Feature::TypeDefinition, watcher).await?,
            ),
            Query::IncomingCalls(lsp::IncomingCalls(at)) => {
                Reply::LspIncomingCalls(self.calls(at, true, watcher).await?)
            }
            Query::OutgoingCalls(lsp::OutgoingCalls(at)) => {
                Reply::LspOutgoingCalls(self.calls(at, false, watcher).await?)
            }
            Query::Diagnostics(request) => {
                Reply::LspDiagnostics(self.diagnostic_query(request, watcher).await?)
            }
        })
    }
}

#[derive(Clone, Copy)]
enum Feature {
    Definition,
    Implementation,
    TypeDefinition,
}

fn supported(provider: Option<&crate::protocol::initialize::Provider>) -> Result<(), Error> {
    if provider.is_some_and(crate::protocol::initialize::Provider::enabled) {
        Ok(())
    } else {
        Err(Error::Unsupported)
    }
}
