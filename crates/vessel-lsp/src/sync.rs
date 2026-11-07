//! Sync bounded saved snapshots; incremental servers receive one full-range edit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::watch;
use vessel_proto::lsp::{FilePath, Language};

use crate::error::LspError as Error;
use crate::position::Utf16Position;
use crate::process::{Document, MAX_DOCUMENTS, Process};

/// Saved text all open documents of one server may hold together.
const MAX_OPEN_BYTES: usize = 32 * 1024 * 1024;
use crate::protocol::data;

/// Coalescing events is safe: each wake rereads the bounded open document set.
#[derive(Debug, Clone)]
pub(crate) struct Watcher(Arc<Mutex<NativeWatches>>);

#[derive(Debug)]
struct NativeWatches {
    watcher: RecommendedWatcher,
    /// Watched parent directories and how many open documents need each.
    parents: BTreeMap<PathBuf, usize>,
}

impl Watcher {
    pub(crate) async fn new() -> Result<(Self, watch::Receiver<bool>), Error> {
        let (sender, receiver) = watch::channel(false);
        let watcher = tokio::task::spawn_blocking(move || {
            notify::recommended_watcher(move |_event: notify::Result<notify::Event>| {
                // Errors/overflows also trigger a reread, never a claim that
                // the saved snapshots are current because no event arrived.
                let _old = sender.send_replace(true);
            })
        })
        .await??;
        Ok((
            Self(Arc::new(Mutex::new(NativeWatches {
                watcher,
                parents: BTreeMap::new(),
            }))),
            receiver,
        ))
    }

    /// Run `change` on the watch set off the async workers.
    async fn with(
        &self,
        change: impl FnOnce(&mut NativeWatches) -> Result<(), Error> + Send + 'static,
    ) -> Result<(), Error> {
        let watches = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            let mut watches = watches.lock().map_err(|_poisoned| Error::Unavailable)?;
            change(&mut watches)
        })
        .await?
    }

    pub(crate) async fn add(&self, path: &Path) -> Result<(), Error> {
        let parent = path.parent().ok_or(Error::Path)?.to_owned();
        // Parent watches survive atomic saves. Nonrecursive bounded watches
        // avoid allocating one watch per dependency or build-output directory.
        self.with(move |watches| {
            if let Some(count) = watches.parents.get_mut(&parent) {
                *count = count.saturating_add(1);
                return Ok(());
            }
            if watches.parents.len() >= MAX_DOCUMENTS {
                return Err(Error::Limit);
            }
            watches
                .watcher
                .watch(&parent, RecursiveMode::NonRecursive)?;
            let _previous = watches.parents.insert(parent, 1);
            Ok(())
        })
        .await
    }

    /// Release one document's need for its parent watch.
    pub(crate) async fn remove(&self, path: &Path) -> Result<(), Error> {
        let parent = path.parent().ok_or(Error::Path)?.to_owned();
        self.with(move |watches| {
            let remaining = watches.parents.get_mut(&parent).map(|count| {
                *count = count.saturating_sub(1);
                *count
            });
            if remaining == Some(0) {
                let _count = watches.parents.remove(&parent);
                // The directory may already be gone; its watch is then too.
                drop(watches.watcher.unwatch(&parent));
            }
            Ok(())
        })
        .await
    }

    /// Forget every watch. A new server generation starts with no open
    /// documents, so counts from the previous one must not leak toward the
    /// bound.
    pub(crate) async fn reset(&self) -> Result<(), Error> {
        self.with(|watches| {
            for parent in std::mem::take(&mut watches.parents).into_keys() {
                drop(watches.watcher.unwatch(&parent));
            }
            Ok(())
        })
        .await
    }
}

impl Process {
    pub(crate) async fn open(
        &mut self,
        file: &FilePath,
        watcher: &Watcher,
    ) -> Result<String, Error> {
        let path = crate::documents::resolve(&self.root, file).await?;
        let uri = crate::documents::uri(&path)?;
        let used = self.tick();
        if let Some(document) = self.documents.get_mut(&uri) {
            document.used = used;
            self.refresh(&uri, watcher).await?;
            return Ok(uri);
        }
        let text = crate::documents::read(&path).await?;
        self.make_room(text.len(), None, watcher).await?;
        let sync = self
            .capabilities
            .text_document_sync
            .as_ref()
            .ok_or(Error::Unsupported)?;
        if !sync.open_close() {
            return Err(Error::Unsupported);
        }
        watcher.add(&path).await?;
        let language_id = language_id(self.language, &path);
        self.notify(
            "textDocument/didOpen",
            data::Open {
                text_document: data::OpenDocument {
                    uri: &uri,
                    language_id,
                    version: 1,
                    text: &text,
                },
            },
        )
        .await?;
        drop(self.documents.insert(
            uri.clone(),
            Document {
                file: file.clone(),
                path,
                text: Arc::from(text),
                version: 1,
                used,
            },
        ));
        Ok(uri)
    }

    /// [Decision Log]
    /// - Purpose: a long-lived server keeps serving new files.
    /// - Before: 64 open documents locked every new file out for as long as
    ///   queries kept the server from idling (review finding).
    /// - Chosen: close least-recently-used documents (`didClose`, drop their
    ///   diagnostics and watch) until the new text fits the count and byte
    ///   budgets. `keep` protects a document being refreshed. A query opens
    ///   at most one document, so the one it uses is never the victim.
    async fn make_room(
        &mut self,
        size: usize,
        keep: Option<&str>,
        watcher: &Watcher,
    ) -> Result<(), Error> {
        loop {
            let count = self
                .documents
                .keys()
                .filter(|uri| Some(uri.as_str()) != keep)
                .count();
            let fits = count < MAX_DOCUMENTS
                && self.open_bytes(keep).saturating_add(size) <= MAX_OPEN_BYTES;
            if fits {
                return Ok(());
            }
            let oldest = self
                .documents
                .iter()
                .filter(|(uri, _)| Some(uri.as_str()) != keep)
                .min_by_key(|(_, document)| document.used)
                .map(|(uri, _)| uri.clone())
                .ok_or(Error::Limit)?;
            self.close(&oldest, watcher).await?;
        }
    }

    pub(crate) async fn refresh_all(&mut self, watcher: &Watcher) -> Result<(), Error> {
        let uris: Vec<_> = self.documents.keys().cloned().collect();
        for uri in uris {
            self.refresh(&uri, watcher).await?;
        }
        Ok(())
    }

    async fn refresh(&mut self, uri: &str, watcher: &Watcher) -> Result<(), Error> {
        let document = self.documents.get(uri).ok_or(Error::Protocol)?;
        // A saved file may have been replaced by an escaping symlink. Close
        // and forget it instead of continuing to expose its old diagnostics.
        let path = match crate::documents::resolve(&self.root, &document.file).await {
            Ok(path) if path == document.path => path,
            Ok(_) | Err(_) => return self.close(uri, watcher).await,
        };
        let text = crate::documents::read(&path).await?;
        if text
            == self
                .documents
                .get(uri)
                .ok_or(Error::Protocol)?
                .text
                .as_ref()
        {
            return Ok(());
        }
        self.make_room(text.len(), Some(uri), watcher).await?;
        let document = self.documents.get(uri).ok_or(Error::Protocol)?;
        let version = document.version.checked_add(1).ok_or(Error::Limit)?;
        let sync_kind = self
            .capabilities
            .text_document_sync
            .as_ref()
            .map_or(0, crate::protocol::initialize::Sync::kind);
        let range = match sync_kind {
            1 => None,
            2 => Some(data::Range {
                start: Utf16Position {
                    line: 0,
                    character: 0,
                },
                end: end(&document.text)?,
            }),
            _ => return Err(Error::Unsupported),
        };
        self.notify(
            "textDocument/didChange",
            data::Change {
                text_document: data::Versioned { uri, version },
                content_changes: vec![data::ContentChange { range, text: &text }],
            },
        )
        .await?;
        let document = self.documents.get_mut(uri).ok_or(Error::Protocol)?;
        document.text = Arc::from(text);
        document.version = version;
        drop(self.diagnostics.remove(uri));
        Ok(())
    }

    async fn close(&mut self, uri: &str, watcher: &Watcher) -> Result<(), Error> {
        let document = self.documents.remove(uri);
        drop(self.diagnostics.remove(uri));
        if let Some(document) = document {
            watcher.remove(&document.path).await?;
        }
        self.notify(
            "textDocument/didClose",
            data::DocumentParams {
                text_document: data::Document {
                    uri: uri.to_owned(),
                },
            },
        )
        .await
    }

    fn open_bytes(&self, except: Option<&str>) -> usize {
        self.documents
            .iter()
            .filter(|(uri, _)| Some(uri.as_str()) != except)
            .map(|(_, document)| document.text.len())
            .fold(0_usize, usize::saturating_add)
    }
}

fn end(text: &str) -> Result<Utf16Position, Error> {
    let mut line = 0_u32;
    let mut character = 0_u32;
    let mut lines = text.split('\n').peekable();
    while let Some(text) = lines.next() {
        let text = text.strip_suffix('\r').unwrap_or(text);
        character = text
            .chars()
            .map(|ch| if u32::from(ch) >= 0x1_0000 { 2_u32 } else { 1 })
            .fold(0_u32, u32::saturating_add);
        if lines.peek().is_some() {
            line = line.checked_add(1).ok_or(Error::Limit)?;
        }
    }
    Ok(Utf16Position { line, character })
}

fn language_id(language: Language, path: &Path) -> &'static str {
    let extension = path.extension().and_then(|extension| extension.to_str());
    match language {
        Language::Rust => "rust",
        Language::Python => "python",
        Language::Go => "go",
        Language::Cpp => {
            if extension == Some("c") {
                "c"
            } else {
                "cpp"
            }
        }
        Language::TypeScript => match extension {
            Some("js" | "mjs" | "cjs") => "javascript",
            Some("jsx") => "javascriptreact",
            Some("tsx") => "typescriptreact",
            _ => "typescript",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_native_saved_file_event_marks_the_open_set_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("watched.rs");
        tokio::fs::write(&file, "before").await.unwrap();
        let (watcher, mut changes) = Watcher::new().await.unwrap();
        watcher.add(&file).await.unwrap();
        drop(changes.borrow_and_update());
        tokio::fs::write(&file, "after").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), changes.changed())
            .await
            .expect("native file change notification")
            .unwrap();
        assert!(*changes.borrow());
    }
}
