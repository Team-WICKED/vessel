//! Exercise the process boundary with an independent fake and temporary roots.

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;
    use vessel_lsp::{Config, LspError, Manager, Query, registry::Override};
    use vessel_proto::{Reply, lsp};
    use vessel_testkit::{Fake, within};

    const SOURCE: &str = "pub fn caller() { target(); }\npub fn target() {}\n";

    struct Fixture {
        dir: TempDir,
        _state: TempDir,
        manager: Manager,
        stop: CancellationToken,
        workspace: lsp::Workspace,
    }
    impl Fixture {
        async fn new(mode: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            tokio::fs::write(dir.path().join("fixture.rs"), SOURCE)
                .await
                .unwrap();
            tokio::fs::write(
                dir.path().join("Cargo.toml"),
                "[package]\nname = \"fixture\"\n",
            )
            .await
            .unwrap();
            let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
            let state = tempfile::tempdir().unwrap();
            let mut config = Config {
                request_timeout: Duration::from_secs(3),
                ..Config::default()
            };
            config.registry.state_dir = Some(state.path().to_owned());
            config.registry.rust_toolchain =
                Some(vessel_testkit::rust_toolchain(state.path()).unwrap());
            let _previous = config.registry.overrides.insert(
                lsp::Language::Rust,
                Override {
                    program: Fake::Lsp.path().unwrap(),
                    args: vec![mode.into()],
                },
            );
            let stop = CancellationToken::new();
            let manager = Manager::start(config, stop.child_token());
            Self {
                dir,
                _state: state,
                manager,
                stop,
                workspace: lsp::Workspace {
                    root,
                    language: lsp::Language::Rust,
                },
            }
        }
        fn file() -> lsp::FilePath {
            lsp::FilePath::try_from(PathBuf::from("fixture.rs")).unwrap()
        }
        fn at(&self) -> lsp::AtPosition {
            lsp::AtPosition {
                workspace: self.workspace.clone(),
                file: Self::file(),
                position: lsp::Position {
                    line: NonZeroU32::new(2).unwrap(),
                    column: NonZeroU32::new(8).unwrap(),
                },
                page: lsp::PageRequest::default(),
            }
        }
        async fn query(&self, query: Query) -> Result<Reply, LspError> {
            self.manager
                .query(self.workspace.root.clone(), false, query)
                .await
        }
        async fn stop(self) {
            self.stop.cancel();
            self.manager.stopped().await;
        }
    }

    #[tokio::test]
    async fn all_read_only_queries_cross_the_real_stdio_boundary() {
        within(Duration::from_secs(45), async {
            let fixture = Fixture::new("normal").await;
            let at = fixture.at();
            let queries = [
                Query::WorkspaceSymbols(lsp::WorkspaceSymbols {
                    workspace: fixture.workspace.clone(),
                    query: "target".into(),
                    page: lsp::PageRequest::default(),
                }),
                Query::DocumentSymbols(lsp::DocumentSymbols {
                    workspace: fixture.workspace.clone(),
                    file: Fixture::file(),
                    page: lsp::PageRequest::default(),
                }),
                Query::Definition(lsp::Definition(at.clone())),
                Query::References(lsp::References {
                    at: at.clone(),
                    include_declaration: true,
                }),
                Query::Hover(lsp::GetHover(at.clone())),
                Query::Implementations(lsp::Implementations(at.clone())),
                Query::TypeDefinition(lsp::TypeDefinition(at.clone())),
                Query::IncomingCalls(lsp::IncomingCalls(at.clone())),
                Query::OutgoingCalls(lsp::OutgoingCalls(at)),
                Query::Diagnostics(lsp::Diagnostics {
                    workspace: fixture.workspace.clone(),
                    file: Some(Fixture::file()),
                    page: lsp::PageRequest::default(),
                }),
            ];
            for query in queries {
                let reply = fixture.query(query).await.unwrap();
                let json = serde_json::to_string(&reply).unwrap();
                assert!(json.contains("fixture.rs"), "{json}");
                assert!(json.contains("observed_idle"), "{json}");
                assert!(
                    !json.contains("keep-opaque"),
                    "server hierarchy data must stay private"
                );
            }
            fixture.stop().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn changed_saved_files_use_incremental_sync_and_refresh_diagnostics() {
        within(Duration::from_secs(45), async {
            let fixture = Fixture::new("incremental").await;
            let symbols = || {
                Query::DocumentSymbols(lsp::DocumentSymbols {
                    workspace: fixture.workspace.clone(),
                    file: Fixture::file(),
                    page: lsp::PageRequest::default(),
                })
            };
            let Reply::LspDocumentSymbols(before) = fixture.query(symbols()).await.unwrap() else {
                panic!("wrong reply");
            };
            assert_eq!(before.items[0].name, "target");
            tokio::fs::write(
                fixture.dir.path().join("fixture.rs"),
                SOURCE.replace("target", "changed") + "// ERROR\n",
            )
            .await
            .unwrap();
            let Reply::LspDocumentSymbols(after) = fixture.query(symbols()).await.unwrap() else {
                panic!("wrong reply");
            };
            assert_eq!(after.items[0].name, "changed");
            let query = Query::Diagnostics(lsp::Diagnostics {
                workspace: fixture.workspace.clone(),
                file: Some(Fixture::file()),
                page: lsp::PageRequest::default(),
            });
            let Reply::LspDiagnostics(diagnostics) = fixture.query(query).await.unwrap() else {
                panic!("wrong reply");
            };
            assert_eq!(diagnostics.items.len(), 1);
            assert_eq!(diagnostics.items[0].message, "fixture diagnostic");
            fixture.stop().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn silence_is_unknown_and_outside_locations_never_become_source_reads() {
        within(Duration::from_secs(45), async {
            let fixture = Fixture::new("silent").await;
            let query = Query::Diagnostics(lsp::Diagnostics {
                workspace: fixture.workspace.clone(),
                file: Some(Fixture::file()),
                page: lsp::PageRequest::default(),
            });
            let Reply::LspDiagnostics(page) = fixture.query(query).await.unwrap() else {
                panic!("wrong reply");
            };
            assert!(page.items.is_empty());
            assert!(page.context.files.is_empty());
            assert_eq!(page.context.freshness, lsp::Freshness::Unknown);
            assert!(page.context.observed_at.is_none());
            assert!(!page.context.workspace_complete);
            fixture.stop().await;
            let fixture = Fixture::new("outside").await;
            let query = Query::WorkspaceSymbols(lsp::WorkspaceSymbols {
                workspace: fixture.workspace.clone(),
                query: "target".into(),
                page: lsp::PageRequest::default(),
            });
            let Reply::LspWorkspaceSymbols(page) = fixture.query(query).await.unwrap() else {
                panic!("wrong reply");
            };
            assert!(page.items.is_empty());
            assert!(page.truncated);
            fixture.stop().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_hung_or_malformed_peer_fails_and_enters_backoff() {
        within(Duration::from_secs(45), async {
            for mode in ["hang", "crash", "oversized"] {
                let fixture = Fixture::new(mode).await;
                let query = Query::Hover(lsp::GetHover(fixture.at()));
                assert!(fixture.query(query.clone()).await.is_err());
                assert!(matches!(
                    fixture.query(query).await,
                    Err(LspError::Unavailable)
                ));
                fixture.stop().await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_refused_before_opening_a_document() {
        within(Duration::from_secs(45), async {
            let fixture = Fixture::new("normal").await;
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.rs"), SOURCE).unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("secret.rs"),
                fixture.dir.path().join("escape.rs"),
            )
            .unwrap();
            let query = Query::DocumentSymbols(lsp::DocumentSymbols {
                workspace: fixture.workspace.clone(),
                file: lsp::FilePath::try_from(PathBuf::from("escape.rs")).unwrap(),
                page: lsp::PageRequest::default(),
            });
            assert!(matches!(fixture.query(query).await, Err(LspError::Path)));
            fixture.stop().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn content_modified_is_retried_without_returning_an_obsolete_snapshot() {
        within(Duration::from_secs(45), async {
            let fixture = Fixture::new("content-modified").await;
            let Reply::LspHover(hover) = fixture
                .query(Query::Hover(lsp::GetHover(fixture.at())))
                .await
                .unwrap()
            else {
                panic!("wrong reply");
            };
            assert_eq!(hover.text, "fn target()");
            fixture.stop().await;
        })
        .await
        .unwrap();
    }
}
