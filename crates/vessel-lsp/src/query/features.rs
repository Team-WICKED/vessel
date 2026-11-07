//! Read-only LSP feature calls, using one saved version per request.

use vessel_proto::lsp;

use super::convert::{Sources, page};
use super::{Feature, supported};
use crate::error::LspError as Error;
use crate::process::{MAX_ITEMS, Process};
use crate::protocol::data;
use crate::sync::Watcher;

impl Process {
    pub(super) async fn at(
        &mut self,
        at: &lsp::AtPosition,
        watcher: &Watcher,
    ) -> Result<data::At, Error> {
        let uri = self.open(&at.file, watcher).await?;
        let document = self.documents.get(&uri).ok_or(Error::Path)?;
        let position = crate::position::to_utf16(&document.text, at.position)?;
        Ok(data::At {
            text_document: data::Document { uri },
            position,
        })
    }

    pub(super) async fn workspace_symbols(
        &mut self,
        request: lsp::WorkspaceSymbols,
    ) -> Result<lsp::Page<lsp::Symbol>, Error> {
        supported(self.capabilities.workspace_symbol_provider.as_ref())?;
        if request.query.len() > 1024 {
            return Err(Error::Limit);
        }
        let symbols: Option<Vec<data::Symbol>> = self
            .request(
                "workspace/symbol",
                data::Search {
                    query: &request.query,
                },
            )
            .await?;
        let mut sources = Sources::new(self);
        let symbols = symbols.unwrap_or_default();
        sources.truncated = symbols.len() > MAX_ITEMS;
        let mut items = Vec::new();
        for symbol in symbols.into_iter().take(MAX_ITEMS) {
            if let Some(symbol) = sources.symbol(symbol).await {
                items.push(symbol);
            }
        }
        page(
            items,
            &request.page,
            sources.context(self),
            sources.truncated,
        )
    }

    pub(super) async fn document_symbols(
        &mut self,
        request: lsp::DocumentSymbols,
        watcher: &Watcher,
    ) -> Result<lsp::Page<lsp::Symbol>, Error> {
        supported(self.capabilities.document_symbol_provider.as_ref())?;
        let uri = self.open(&request.file, watcher).await?;
        let result: Option<data::DocumentSymbols> = self
            .request(
                "textDocument/documentSymbol",
                data::DocumentParams {
                    text_document: data::Document { uri: uri.clone() },
                },
            )
            .await?;
        let mut sources = Sources::new(self);
        sources.include(request.file);
        let symbols = match result {
            Some(data::DocumentSymbols::Flat(symbols)) => symbols,
            Some(data::DocumentSymbols::Nested(symbols)) => flatten(symbols, &uri),
            None => Vec::new(),
        };
        sources.truncated = symbols.len() > MAX_ITEMS;
        let mut items = Vec::new();
        for symbol in symbols.into_iter().take(MAX_ITEMS) {
            if let Some(symbol) = sources.symbol(symbol).await {
                items.push(symbol);
            }
        }
        page(
            items,
            &request.page,
            sources.context(self),
            sources.truncated,
        )
    }

    pub(super) async fn locations(
        &mut self,
        at: lsp::AtPosition,
        feature: Feature,
        watcher: &Watcher,
    ) -> Result<lsp::Page<lsp::Location>, Error> {
        let (provider, method) = match feature {
            Feature::Definition => (
                &self.capabilities.definition_provider,
                "textDocument/definition",
            ),
            Feature::Implementation => (
                &self.capabilities.implementation_provider,
                "textDocument/implementation",
            ),
            Feature::TypeDefinition => (
                &self.capabilities.type_definition_provider,
                "textDocument/typeDefinition",
            ),
        };
        supported(provider.as_ref())?;
        let params = self.at(&at, watcher).await?;
        let result: Option<data::Locations> = self.request(method, params).await?;
        self.location_page(result, &at).await
    }

    pub(super) async fn references(
        &mut self,
        request: lsp::References,
        watcher: &Watcher,
    ) -> Result<lsp::Page<lsp::Location>, Error> {
        supported(self.capabilities.references_provider.as_ref())?;
        let at = self.at(&request.at, watcher).await?;
        let result = self
            .request(
                "textDocument/references",
                data::ReferenceParams {
                    at,
                    context: data::ReferenceContext {
                        include_declaration: request.include_declaration,
                    },
                },
            )
            .await?;
        self.location_page(result, &request.at).await
    }

    async fn location_page(
        &self,
        result: Option<data::Locations>,
        at: &lsp::AtPosition,
    ) -> Result<lsp::Page<lsp::Location>, Error> {
        let locations = match result {
            Some(data::Locations::One(location)) => vec![location],
            Some(data::Locations::Many(locations)) => locations,
            Some(data::Locations::Links(links)) => links
                .into_iter()
                .map(|link| data::Location {
                    uri: link.target_uri,
                    range: link.target_selection_range,
                })
                .collect(),
            None => Vec::new(),
        };
        let mut sources = Sources::new(self);
        sources.include(at.file.clone());
        sources.truncated = locations.len() > MAX_ITEMS;
        let mut items = Vec::new();
        for location in locations.into_iter().take(MAX_ITEMS) {
            if let Some(location) = sources.location(&location.uri, location.range).await {
                items.push(location);
            }
        }
        page(items, &at.page, sources.context(self), sources.truncated)
    }

    pub(super) async fn hover(
        &mut self,
        at: lsp::AtPosition,
        watcher: &Watcher,
    ) -> Result<lsp::Hover, Error> {
        supported(self.capabilities.hover_provider.as_ref())?;
        let params = self.at(&at, watcher).await?;
        let uri = params.text_document.uri.clone();
        let result: Option<data::Hover> = self.request("textDocument/hover", params).await?;
        let mut sources = Sources::new(self);
        sources.include(at.file);
        let (mut text, format, range) = match result {
            Some(result) => {
                let (text, format) = hover_text(result.contents);
                let range = if let Some(range) = result.range {
                    sources
                        .location(&uri, range)
                        .await
                        .map(|location| location.range)
                } else {
                    None
                };
                (text, format, range)
            }
            None => (String::new(), lsp::Markup::PlainText, None),
        };
        sources.truncated |= super::truncate(&mut text, 16 * 1024);
        Ok(lsp::Hover {
            text,
            format,
            range,
            truncated: sources.truncated,
            context: sources.context(self),
        })
    }

    pub(super) async fn calls(
        &mut self,
        at: lsp::AtPosition,
        incoming: bool,
        watcher: &Watcher,
    ) -> Result<lsp::Page<lsp::Call>, Error> {
        supported(self.capabilities.call_hierarchy_provider.as_ref())?;
        let params = self.at(&at, watcher).await?;
        let prepared: Option<Vec<data::HierarchyItem>> = self
            .request("textDocument/prepareCallHierarchy", params)
            .await?;
        let prepared = prepared.unwrap_or_default();
        let mut sources = Sources::new(self);
        sources.include(at.file.clone());
        sources.truncated = prepared.len() > 16;
        let mut items = Vec::new();
        for item in prepared.into_iter().take(16) {
            // A server-selected hierarchy item must remain in the workspace
            // before we send its opaque data back for a follow-up request.
            if sources.hierarchy(&item).await.is_none() {
                continue;
            }
            let caller_uri = item.uri.clone();
            let edges: Vec<(data::HierarchyItem, String, Vec<data::Range>)> = if incoming {
                let calls: Option<Vec<data::IncomingCall>> = self
                    .request("callHierarchy/incomingCalls", data::Hierarchy { item })
                    .await?;
                calls
                    .unwrap_or_default()
                    .into_iter()
                    .map(|call| {
                        let uri = call.from.uri.clone();
                        (call.from, uri, call.from_ranges)
                    })
                    .collect()
            } else {
                let calls: Option<Vec<data::OutgoingCall>> = self
                    .request("callHierarchy/outgoingCalls", data::Hierarchy { item })
                    .await?;
                calls
                    .unwrap_or_default()
                    .into_iter()
                    .map(|call| (call.to, caller_uri.clone(), call.from_ranges))
                    .collect()
            };
            sources.truncated |= edges.len() > MAX_ITEMS.saturating_sub(items.len());
            for (symbol, uri, ranges) in edges
                .into_iter()
                .take(MAX_ITEMS.saturating_sub(items.len()))
            {
                let Some(symbol) = sources.hierarchy(&symbol).await else {
                    continue;
                };
                sources.truncated |= ranges.len() > 200;
                let mut sites = Vec::new();
                for range in ranges.into_iter().take(200) {
                    if let Some(site) = sources.location(&uri, range).await {
                        sites.push(site);
                    }
                }
                items.push(lsp::Call { symbol, sites });
            }
        }
        page(items, &at.page, sources.context(self), sources.truncated)
    }
}

fn flatten(symbols: Vec<data::DocumentSymbol>, uri: &str) -> Vec<data::Symbol> {
    let mut queue: Vec<_> = symbols.into_iter().map(|symbol| (symbol, None)).collect();
    let mut flat = Vec::new();
    while let Some((symbol, container)) = queue.pop() {
        for child in symbol.children {
            queue.push((child, Some(symbol.name.clone())));
        }
        flat.push(data::Symbol {
            name: symbol.name,
            kind: symbol.kind,
            location: data::Location {
                uri: uri.to_owned(),
                range: symbol.selection_range,
            },
            container_name: container,
        });
        // One extra signals truncation to the caller without traversing more.
        if flat.len() > MAX_ITEMS {
            break;
        }
    }
    flat
}

fn hover_text(contents: data::HoverContents) -> (String, lsp::Markup) {
    match contents {
        data::HoverContents::Markup(markup) => (
            markup.value,
            match markup.kind {
                data::MarkupKind::Plaintext => lsp::Markup::PlainText,
                data::MarkupKind::Markdown => lsp::Markup::Markdown,
            },
        ),
        data::HoverContents::One(marked) => (marked_text(marked), lsp::Markup::Markdown),
        data::HoverContents::Many(marked) => (
            marked
                .into_iter()
                .map(marked_text)
                .collect::<Vec<_>>()
                .join("\n\n"),
            lsp::Markup::Markdown,
        ),
    }
}

fn marked_text(marked: data::MarkedString) -> String {
    match marked {
        data::MarkedString::Text(text) => text,
        data::MarkedString::Code { language, value } => format!("```{language}\n{value}\n```"),
    }
}
