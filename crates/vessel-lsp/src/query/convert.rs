//! Filter and convert every location before reading server-selected source.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use vessel_proto::lsp;

use crate::error::LspError as Error;
use crate::process::{MAX_DOCUMENTS, MAX_ITEMS, Process};
use crate::protocol::data;

pub(super) struct Sources {
    root: PathBuf,
    texts: BTreeMap<String, Arc<str>>,
    files: BTreeSet<lsp::FilePath>,
    pub(super) truncated: bool,
}

impl Sources {
    pub(super) fn new(process: &Process) -> Self {
        Self {
            root: process.root.clone(),
            texts: process
                .documents
                .iter()
                .map(|(uri, doc)| (uri.clone(), Arc::clone(&doc.text)))
                .collect(),
            files: BTreeSet::new(),
            truncated: false,
        }
    }

    pub(super) fn include(&mut self, file: lsp::FilePath) {
        let _inserted = self.files.insert(file);
    }

    pub(super) async fn location(
        &mut self,
        uri: &str,
        range: data::Range,
    ) -> Option<lsp::Location> {
        // A malformed, outside-root or missing result is omitted without
        // revealing the rejected URI or why it was withheld.
        if let Ok(location) = self.location_inner(uri, range).await {
            Some(location)
        } else {
            self.truncated = true;
            None
        }
    }

    async fn location_inner(
        &mut self,
        uri: &str,
        range: data::Range,
    ) -> Result<lsp::Location, Error> {
        let (file, path) = crate::documents::from_uri(&self.root, uri).await?;
        if !self.texts.contains_key(uri) {
            if self.texts.len() >= MAX_DOCUMENTS {
                return Err(Error::Limit);
            }
            let text = crate::documents::read(&path).await?;
            let bytes = self
                .texts
                .values()
                .map(|text| text.len())
                .fold(text.len(), usize::saturating_add);
            if bytes > 32 * 1024 * 1024 {
                return Err(Error::Limit);
            }
            drop(self.texts.insert(uri.to_owned(), Arc::from(text)));
        }
        let text = self.texts.get(uri).ok_or(Error::Protocol)?;
        let start = crate::position::from_utf16(text, range.start)?;
        let end = crate::position::from_utf16(text, range.end)?;
        if end < start {
            return Err(Error::Protocol);
        }
        let _inserted = self.files.insert(file.clone());
        Ok(lsp::Location {
            file,
            range: lsp::Range { start, end },
        })
    }

    pub(super) fn context(&self, process: &Process) -> lsp::Context {
        lsp::Context {
            files: self.files.iter().cloned().collect(),
            observed_at: Some(Utc::now()),
            indexing: process.indexing(),
            freshness: lsp::Freshness::Saved,
            workspace_complete: false,
        }
    }

    pub(super) async fn symbol(&mut self, symbol: data::Symbol) -> Option<lsp::Symbol> {
        let location = self
            .location(&symbol.location.uri, symbol.location.range)
            .await?;
        let mut name = symbol.name;
        self.truncated |= truncate(&mut name, 1024);
        let mut container = symbol.container_name;
        if let Some(container) = &mut container {
            self.truncated |= truncate(container, 1024);
        }
        Some(lsp::Symbol {
            name,
            kind: kind(symbol.kind),
            location,
            container,
        })
    }

    pub(super) async fn hierarchy(&mut self, item: &data::HierarchyItem) -> Option<lsp::Symbol> {
        self.symbol(data::Symbol {
            name: item.name.clone(),
            kind: item.kind,
            location: data::Location {
                uri: item.uri.clone(),
                range: item.selection_range,
            },
            container_name: None,
        })
        .await
    }
}

pub(super) fn page<T: Ord + Serialize>(
    mut items: Vec<T>,
    request: &lsp::PageRequest,
    context: lsp::Context,
    mut truncated: bool,
) -> Result<lsp::Page<T>, Error> {
    if items.len() > MAX_ITEMS {
        items.truncate(MAX_ITEMS);
        truncated = true;
    }
    items.sort();
    items.dedup();
    let offset = usize::try_from(request.offset.0).unwrap_or(usize::MAX);
    let limit = usize::try_from(request.limit.get().min(200)).unwrap_or(200);
    let mut page = Vec::new();
    let mut bytes = 0_usize;
    let mut budget_remainder = false;
    let mut remaining = items.into_iter().skip(offset).peekable();
    while page.len() < limit {
        let Some(item) = remaining.next() else {
            break;
        };
        let size = serde_json::to_vec(&item)?.len();
        if bytes.saturating_add(size) > 384 * 1024 {
            truncated = true;
            budget_remainder = true;
            break;
        }
        bytes = bytes.saturating_add(size);
        page.push(item);
    }
    let next = if (remaining.peek().is_some() || budget_remainder) && !page.is_empty() {
        Some(lsp::Offset(
            request
                .offset
                .0
                .saturating_add(u32::try_from(page.len()).unwrap_or(200)),
        ))
    } else {
        None
    };
    Ok(lsp::Page {
        items: page,
        next,
        truncated,
        context,
    })
}

pub(crate) fn truncate(text: &mut String, maximum: usize) -> bool {
    if text.len() <= maximum {
        return false;
    }
    let mut boundary = maximum;
    while !text.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    text.truncate(boundary);
    true
}

pub(super) const fn kind(value: u32) -> lsp::SymbolKind {
    use lsp::SymbolKind as K;
    match value {
        1 => K::File,
        2 => K::Module,
        3 => K::Namespace,
        4 => K::Package,
        5 => K::Class,
        6 => K::Method,
        7 => K::Property,
        8 => K::Field,
        9 => K::Constructor,
        10 => K::Enum,
        11 => K::Interface,
        12 => K::Function,
        13 => K::Variable,
        14 => K::Constant,
        15 => K::String,
        16 => K::Number,
        17 => K::Boolean,
        18 => K::Array,
        19 => K::Object,
        20 => K::Key,
        21 => K::Null,
        22 => K::EnumMember,
        23 => K::Struct,
        24 => K::Event,
        25 => K::Operator,
        26 => K::TypeParameter,
        _ => K::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> lsp::Context {
        lsp::Context {
            files: Vec::new(),
            observed_at: None,
            indexing: lsp::Indexing::Unknown,
            freshness: lsp::Freshness::Unknown,
            workspace_complete: false,
        }
    }
    #[test]
    fn pages_sort_deduplicate_and_keep_the_next_offset() {
        let request = lsp::PageRequest {
            offset: lsp::Offset(0),
            limit: std::num::NonZeroU32::new(2).unwrap(),
        };
        let first = page(vec![3, 1, 2, 2], &request, context(), false).unwrap();
        assert_eq!(first.items, [1, 2]);
        assert_eq!(first.next, Some(lsp::Offset(2)));
        let second = page(
            vec![3, 1, 2, 2],
            &lsp::PageRequest {
                offset: first.next.unwrap(),
                ..request
            },
            context(),
            false,
        )
        .unwrap();
        assert_eq!(second.items, [3]);
        assert!(second.next.is_none());
    }
    #[test]
    fn an_oversized_item_is_explicitly_truncated() {
        let result = page(
            vec!["x".repeat(512 * 1024)],
            &lsp::PageRequest::default(),
            context(),
            false,
        )
        .unwrap();
        assert!(result.items.is_empty());
        assert!(result.truncated);
    }
}
