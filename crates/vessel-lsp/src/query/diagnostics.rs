//! Diagnostic coverage follows actual reports; an empty cache is unknown.

use vessel_proto::lsp;

use super::convert::{Sources, page};
use crate::error::LspError as Error;
use crate::process::Process;
use crate::protocol::data;
use crate::sync::Watcher;

impl Process {
    async fn pull_diagnostics(&mut self, uri: Option<&String>) -> Result<bool, Error> {
        let mut complete = false;
        if let Some(provider) = &self.capabilities.diagnostic_provider {
            let identifier = provider.identifier.clone();
            if let Some(uri) = uri {
                let result: data::DiagnosticReport = self
                    .request(
                        "textDocument/diagnostic",
                        data::Pull {
                            text_document: data::Document { uri: uri.clone() },
                            identifier,
                        },
                    )
                    .await?;
                if let data::DiagnosticReport::Full { items } = result {
                    let version = self.documents.get(uri).map(|document| document.version);
                    self.cache(data::Published {
                        uri: uri.clone(),
                        version,
                        diagnostics: items,
                    })
                    .await?;
                }
            } else if provider.workspace_diagnostics {
                let reports: data::WorkspaceDiagnostics = self
                    .request(
                        "workspace/diagnostic",
                        data::WorkspacePull {
                            identifier,
                            previous_result_ids: Vec::new(),
                        },
                    )
                    .await?;
                complete = true;
                for report in reports.items {
                    if let data::DiagnosticReport::Full { items } = report.report {
                        self.cache(data::Published {
                            uri: report.uri,
                            version: report.version,
                            diagnostics: items,
                        })
                        .await?;
                    }
                }
            }
        }
        Ok(complete)
    }

    pub(super) async fn diagnostic_query(
        &mut self,
        request: lsp::Diagnostics,
        watcher: &Watcher,
    ) -> Result<lsp::Page<lsp::Diagnostic>, Error> {
        let uri = if let Some(file) = &request.file {
            Some(self.open(file, watcher).await?)
        } else {
            None
        };
        let complete = self.pull_diagnostics(uri.as_ref()).await?;
        self.ready().await?;
        let mut sources = Sources::new(self);
        if let Some(file) = &request.file {
            sources.include(file.clone());
        }
        let mut items = Vec::new();
        let mut observed_at = None;
        let mut covered = Vec::new();
        for (given, report) in &self.diagnostics {
            if uri.as_ref().is_some_and(|uri| uri != given) {
                continue;
            }
            if let Some(document) = self.documents.get(given)
                && report
                    .version
                    .is_some_and(|version| version != document.version)
            {
                continue;
            }
            let Ok((file, _)) = crate::documents::from_uri(&self.root, given).await else {
                continue;
            };
            covered.push(file);
            observed_at = Some(
                observed_at.map_or(report.observed_at, |old: chrono::DateTime<chrono::Utc>| {
                    old.min(report.observed_at)
                }),
            );
            for diagnostic in &report.items {
                let Some(location) = sources.location(given, diagnostic.range).await else {
                    continue;
                };
                let severity = match diagnostic.severity {
                    Some(1) => lsp::Severity::Error,
                    Some(2) => lsp::Severity::Warning,
                    Some(3) => lsp::Severity::Information,
                    Some(4) => lsp::Severity::Hint,
                    _ => lsp::Severity::Unknown,
                };
                items.push(lsp::Diagnostic {
                    location,
                    severity,
                    message: diagnostic.message.clone(),
                    source: diagnostic.source.clone(),
                });
            }
        }
        covered.sort();
        covered.dedup();
        let truncated = sources.truncated || self.cache_truncated;
        let context = lsp::Context {
            files: covered,
            observed_at,
            indexing: self.indexing(),
            freshness: if observed_at.is_some() {
                lsp::Freshness::Cached
            } else {
                lsp::Freshness::Unknown
            },
            workspace_complete: complete && !truncated,
        };
        page(items, &request.page, context, truncated)
    }
}
