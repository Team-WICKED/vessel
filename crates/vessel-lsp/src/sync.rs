//! Sync bounded saved snapshots; incremental servers receive one full-range edit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::watch;
use vessel_proto::lsp::{FilePath, Language};

use crate::error::LspError as Error;
use crate::position::Utf16Position;
use crate::process::{Document, MAX_DOCUMENTS, Process};
use crate::protocol::data;

/// Coalescing events is safe: each wake rereads the bounded open document set.
#[derive(Debug, Clone)]
pub(crate) struct Watcher(Arc<Mutex<NativeWatches>>);

#[derive(Debug)]
struct NativeWatches {
    watcher: RecommendedWatcher,
    parents: BTreeSet<PathBuf>,
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
                parents: BTreeSet::new(),
            }))),
            receiver,
        ))
    }

    pub(crate) async fn add(&self, path: &Path) -> Result<(), Error> {
        let parent = path.parent().ok_or(Error::Path)?.to_owned();
        let watcher = Arc::clone(&self.0);
        // Parent watches survive atomic saves. Nonrecursive bounded watches
        // avoid allocating one watch per dependency or build-output directory.
        tokio::task::spawn_blocking(move || {
            let mut watcher = watcher.lock().map_err(|_poisoned| Error::Unavailable)?;
            if watcher.parents.contains(&parent) {
                return Ok(());
            }
            if watcher.parents.len() >= MAX_DOCUMENTS {
                return Err(Error::Limit);
            }
            watcher
                .watcher
                .watch(&parent, RecursiveMode::NonRecursive)?;
            let _inserted = watcher.parents.insert(parent);
            Ok::<(), Error>(())
        })
        .await?
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
        if self.documents.contains_key(&uri) {
            self.refresh(&uri).await?;
            return Ok(uri);
        }
        if self.documents.len() >= MAX_DOCUMENTS {
            return Err(Error::Limit);
        }
        let text = crate::documents::read(&path).await?;
        self.document_budget(text.len(), None)?;
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
            },
        ));
        Ok(uri)
    }

    pub(crate) async fn refresh_all(&mut self) -> Result<(), Error> {
        let uris: Vec<_> = self.documents.keys().cloned().collect();
        for uri in uris {
            self.refresh(&uri).await?;
        }
        Ok(())
    }

    async fn refresh(&mut self, uri: &str) -> Result<(), Error> {
        let document = self.documents.get(uri).ok_or(Error::Protocol)?;
        // A saved file may have been replaced by an escaping symlink. Close
        // and forget it instead of continuing to expose its old diagnostics.
        let path = match crate::documents::resolve(&self.root, &document.file).await {
            Ok(path) if path == document.path => path,
            Ok(_) | Err(_) => return self.close(uri).await,
        };
        let text = crate::documents::read(&path).await?;
        if text == document.text.as_ref() {
            return Ok(());
        }
        self.document_budget(text.len(), Some(uri))?;
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

    async fn close(&mut self, uri: &str) -> Result<(), Error> {
        drop(self.documents.remove(uri));
        drop(self.diagnostics.remove(uri));
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

    fn document_budget(&self, new_size: usize, replacing: Option<&str>) -> Result<(), Error> {
        let current = self
            .documents
            .iter()
            .filter(|(uri, _)| Some(uri.as_str()) != replacing)
            .map(|(_, doc)| doc.text.len())
            .fold(0_usize, usize::saturating_add);
        if current.saturating_add(new_size) > 32 * 1024 * 1024 {
            return Err(Error::Limit);
        }
        Ok(())
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
