//! Repository traps exercise the actual launch boundary, using only fakes.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use vessel_lsp::{Config, LspError, Manager, Query, registry::Override};
    use vessel_proto::lsp;
    use vessel_testkit::{Fake, within};

    /// Test configuration with a node-owned state directory.
    fn config(state: &tempfile::TempDir) -> Config {
        let mut config = Config::default();
        config.registry.state_dir = Some(state.path().to_owned());
        config.registry.rust_toolchain =
            Some(vessel_testkit::rust_toolchain(state.path()).unwrap());
        config
    }

    fn request(root: PathBuf, language: lsp::Language) -> Query {
        Query::DocumentSymbols(lsp::DocumentSymbols {
            workspace: lsp::Workspace { root, language },
            file: lsp::FilePath::try_from(PathBuf::from("fixture.rs")).unwrap(),
            page: lsp::PageRequest::default(),
        })
    }

    #[tokio::test]
    async fn executable_rust_typescript_and_python_configuration_is_refused_before_spawn() {
        within(Duration::from_secs(45), async {
            for (language, name, contents) in [
                (
                    lsp::Language::Rust,
                    ".cargo/config.toml",
                    "[env]\nLD_PRELOAD={value='evil.so',force=true}\n",
                ),
                (
                    lsp::Language::Rust,
                    ".cargo/config.toml",
                    "[build]\nrustc-wrapper='./evil'\n",
                ),
                (
                    lsp::Language::Rust,
                    ".cargo/config.toml",
                    "[alias]\nmetadata='!./evil'\n",
                ),
                (
                    lsp::Language::Rust,
                    "rust-toolchain.toml",
                    "[toolchain]\npath='./evil'\n",
                ),
                (
                    lsp::Language::TypeScript,
                    "tsconfig.json",
                    r#"{"compilerOptions":{"plugins":[{"name":"./evil"}]}}"#,
                ),
                (
                    lsp::Language::TypeScript,
                    "tsconfig.json",
                    r#"{"compilerOptions":{"\u0070lugins":[]}}"#,
                ),
                (
                    lsp::Language::Python,
                    "pyrightconfig.json",
                    r#"{"venvPath":"./venv","venv":"evil"}"#,
                ),
                (
                    lsp::Language::Python,
                    "pyproject.toml",
                    "[tool.pyright]\npythonPath='./evil'\n",
                ),
            ] {
                let root = tempfile::tempdir().unwrap();
                std::fs::write(
                    root.path().join("fixture.rs"),
                    "pub fn caller() {}\npub fn target() {}\n",
                )
                .unwrap();
                std::fs::write(
                    root.path().join("Cargo.toml"),
                    "[package]\nname='fixture'\n",
                )
                .unwrap();
                let path = root.path().join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, contents).unwrap();
                let marker = root.path().join("server-started");
                let state = tempfile::tempdir().unwrap();
                let mut config = config(&state);
                let _previous = config.registry.overrides.insert(
                    language,
                    Override {
                        program: Fake::Lsp.path().unwrap(),
                        args: vec![
                            "normal".into(),
                            "--record".into(),
                            marker.to_string_lossy().into_owned(),
                        ],
                    },
                );
                let stop = CancellationToken::new();
                let manager = Manager::start(config, stop.child_token());
                let canonical = root.path().canonicalize().unwrap();
                assert!(
                    matches!(
                        manager
                            .query(canonical.clone(), false, request(canonical, language))
                            .await,
                        Err(LspError::ProjectConfiguration)
                    ),
                    "{name}: {contents}"
                );
                assert!(!marker.exists());
                stop.cancel();
                manager.stopped().await;
            }
        })
        .await
        .unwrap();
    }

    /// Review finding (high): a root without its own manifest lets the
    /// server use `sub/Cargo.toml`, and Cargo/rustup then read `sub/`'s
    /// configuration. Every level from such a project up to the root is
    /// checked before anything starts.
    #[tokio::test]
    async fn child_cargo_project_configuration_is_refused_before_spawn() {
        within(Duration::from_secs(45), async {
            for (name, contents) in [
                (".cargo/config.toml", "[build]\nrustc-wrapper='./evil'\n"),
                ("rust-toolchain.toml", "[toolchain]\npath='./evil'\n"),
                ("rust-analyzer.toml", "[cargo.buildScripts]\nenable=true\n"),
            ] {
                let root = tempfile::tempdir().unwrap();
                let sub = root.path().join("sub");
                std::fs::create_dir_all(sub.join(name).parent().unwrap()).unwrap();
                std::fs::write(sub.join("Cargo.toml"), "[package]\nname='sub'\n").unwrap();
                std::fs::write(sub.join("fixture.rs"), "pub fn target() {}\n").unwrap();
                std::fs::write(sub.join(name), contents).unwrap();
                let marker = root.path().join("server-started");
                let state = tempfile::tempdir().unwrap();
                let mut config = config(&state);
                let _previous = config.registry.overrides.insert(
                    lsp::Language::Rust,
                    Override {
                        program: Fake::Lsp.path().unwrap(),
                        args: vec![
                            "normal".into(),
                            "--record".into(),
                            marker.to_string_lossy().into_owned(),
                        ],
                    },
                );
                let stop = CancellationToken::new();
                let manager = Manager::start(config, stop.child_token());
                let canonical = root.path().canonicalize().unwrap();
                let query = Query::DocumentSymbols(lsp::DocumentSymbols {
                    workspace: lsp::Workspace {
                        root: canonical.clone(),
                        language: lsp::Language::Rust,
                    },
                    file: lsp::FilePath::try_from(PathBuf::from("sub/fixture.rs")).unwrap(),
                    page: lsp::PageRequest::default(),
                });
                let outcome = manager.query(canonical.clone(), false, query).await;
                assert!(
                    matches!(outcome, Err(LspError::ProjectConfiguration)),
                    "{name}: {outcome:?}"
                );
                assert!(!marker.exists(), "{name}: the server started");
                stop.cancel();
                manager.stopped().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn go_and_cpp_launch_with_no_repository_execution_path() {
        within(Duration::from_secs(45), async {
            for (language, mode, name, contents) in [
                (lsp::Language::Go, "guard-go", "go.mod", "module example.invalid/project\n\ngo 1.99\ntoolchain go1.99.1\n"),
                (lsp::Language::Cpp, "guard-cpp", "compile_commands.json", r#"[{"directory":".","file":"fixture.rs","command":"./trap-driver -c fixture.rs"}]"#),
            ] {
                let root = tempfile::tempdir().unwrap();
                let canonical = root.path().canonicalize().unwrap();
                std::fs::write(root.path().join("fixture.rs"), "pub fn caller() {}\npub fn target() {}\n").unwrap();
                std::fs::write(root.path().join(name), contents).unwrap();
                let bin = root.path().join("bin"); std::fs::create_dir(&bin).unwrap();
                for trap in ["go", "trap-driver"] {
                    let path = bin.join(trap);
                    std::fs::write(&path, "#!/bin/sh\nprintf ran > trap-was-run\n").unwrap();
                    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt as _; std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap(); }
                }
                let state = tempfile::tempdir().unwrap();
                let mut config = config(&state);
                config.registry.search_path = std::env::join_paths([PathBuf::from("."), bin]).unwrap();
                let _previous = config.registry.overrides.insert(language, Override { program: Fake::Lsp.path().unwrap(), args: vec![mode.into()] });
                let stop = CancellationToken::new(); let manager = Manager::start(config, stop.child_token());
                assert!(manager.query(canonical.clone(), false, request(canonical, language)).await.is_ok());
                assert!(!root.path().join("trap-was-run").exists());
                stop.cancel(); manager.stopped().await;
            }
        }).await.unwrap();
    }
}

/// What a fake server recorded about its own launch (`--record-launch`).
#[cfg(test)]
mod launch {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use vessel_lsp::{Config, LspError, Manager, Query, registry::Override};
    use vessel_proto::{Reply, lsp};
    use vessel_testkit::{Fake, within};

    struct Run {
        outcome: Result<Reply, LspError>,
        record: BTreeMap<String, String>,
    }

    /// Start `language` on the fake with a launch record, ask for the
    /// symbols of `file`, and return the outcome and the record.
    async fn run(
        config: Config,
        mode: &str,
        record: &Path,
        root: &Path,
        language: lsp::Language,
        file: &str,
    ) -> Run {
        let mut config = config;
        let _previous = config.registry.overrides.insert(
            language,
            Override {
                program: Fake::Lsp.path().unwrap(),
                args: vec![
                    mode.into(),
                    "--record-launch".into(),
                    record.to_string_lossy().into_owned(),
                ],
            },
        );
        let stop = CancellationToken::new();
        let manager = Manager::start(config, stop.child_token());
        let query = Query::DocumentSymbols(lsp::DocumentSymbols {
            workspace: lsp::Workspace {
                root: root.to_owned(),
                language,
            },
            file: lsp::FilePath::try_from(PathBuf::from(file)).unwrap(),
            page: lsp::PageRequest::default(),
        });
        let outcome = manager.query(root.to_owned(), false, query).await;
        stop.cancel();
        manager.stopped().await;
        let record = std::fs::read_to_string(record)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        Run { outcome, record }
    }

    fn same(recorded: Option<&String>, expected: &Path) -> bool {
        recorded.is_some_and(|recorded| {
            std::fs::canonicalize(recorded).ok() == std::fs::canonicalize(expected).ok()
        })
    }

    fn config(state: &Path) -> Config {
        let mut config = Config {
            request_timeout: Duration::from_secs(5),
            ..Config::default()
        };
        config.registry.state_dir = Some(state.to_owned());
        config
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PythonSection {
        python_path: PathBuf,
    }

    #[derive(serde::Deserialize)]
    struct RustOptions {
        #[serde(rename = "linkedProjects")]
        linked_projects: Vec<PathBuf>,
    }

    /// Review finding (high), the toolchain layer: a conservative Rust
    /// server gets absolute rustc/cargo and `RUSTUP_TOOLCHAIN`, so no proxy
    /// consults a toolchain file; it runs in the node's own empty directory
    /// with the node's temporary directory; and it may load exactly the
    /// checked child project, nothing it discovers itself.
    #[tokio::test]
    async fn conservative_rust_launches_with_pinned_tools_and_linked_projects() {
        within(Duration::from_secs(45), async {
            let state = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            let root = repo.path().canonicalize().unwrap();
            let sub = root.join("sub");
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("Cargo.toml"), "[package]\nname='sub'\n").unwrap();
            std::fs::write(sub.join("fixture.rs"), "pub fn target() {}\n").unwrap();
            let toolchain = vessel_testkit::rust_toolchain(state.path()).unwrap();
            let mut config = config(state.path());
            config.registry.rust_toolchain = Some(toolchain.clone());
            let record = state.path().join("launch");
            let run = run(
                config,
                "normal",
                &record,
                &root,
                lsp::Language::Rust,
                "sub/fixture.rs",
            )
            .await;
            assert!(run.outcome.is_ok(), "{:?}", run.outcome);
            let exe = std::env::consts::EXE_SUFFIX;
            let record = &run.record;
            assert!(
                same(record.get("cwd"), &state.path().join("cwd")),
                "{record:?}"
            );
            assert!(
                same(record.get("TMPDIR"), &state.path().join("tmp")),
                "{record:?}"
            );
            assert!(same(
                record.get("RUSTC"),
                &toolchain.join(format!("bin/rustc{exe}"))
            ));
            assert!(same(
                record.get("CARGO"),
                &toolchain.join(format!("bin/cargo{exe}"))
            ));
            assert!(
                same(record.get("RUSTUP_TOOLCHAIN"), &toolchain),
                "{record:?}"
            );
            let options: RustOptions = serde_json::from_str(&record["init"]).unwrap();
            assert_eq!(options.linked_projects.len(), 1, "{record:?}");
            assert!(same(
                options
                    .linked_projects
                    .first()
                    .map(|path| path.to_string_lossy().into_owned())
                    .as_ref(),
                &sub.join("Cargo.toml")
            ));
        })
        .await
        .unwrap();
    }

    /// A conservative Rust toolchain inside the served root is refused.
    #[tokio::test]
    async fn a_toolchain_inside_the_root_is_refused() {
        within(Duration::from_secs(45), async {
            let state = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            let root = repo.path().canonicalize().unwrap();
            std::fs::write(root.join("Cargo.toml"), "[package]\nname='fixture'\n").unwrap();
            std::fs::write(root.join("fixture.rs"), "pub fn target() {}\n").unwrap();
            let mut config = config(state.path());
            config.registry.rust_toolchain = Some(vessel_testkit::rust_toolchain(&root).unwrap());
            let record = state.path().join("launch");
            let run = run(
                config,
                "normal",
                &record,
                &root,
                lsp::Language::Rust,
                "fixture.rs",
            )
            .await;
            assert!(
                matches!(run.outcome, Err(LspError::ProjectConfiguration)),
                "{:?}",
                run.outcome
            );
            assert!(!record.exists());
        })
        .await
        .unwrap();
    }

    /// A root with no Cargo project is explicitly unavailable for Rust.
    #[tokio::test]
    async fn a_root_without_a_cargo_project_is_unavailable_for_rust() {
        within(Duration::from_secs(45), async {
            let state = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            let root = repo.path().canonicalize().unwrap();
            std::fs::write(root.join("fixture.rs"), "pub fn target() {}\n").unwrap();
            let mut config = config(state.path());
            config.registry.rust_toolchain =
                Some(vessel_testkit::rust_toolchain(state.path()).unwrap());
            let record = state.path().join("launch");
            let run = run(
                config,
                "normal",
                &record,
                &root,
                lsp::Language::Rust,
                "fixture.rs",
            )
            .await;
            assert!(
                matches!(run.outcome, Err(LspError::NoProject)),
                "{:?}",
                run.outcome
            );
            assert!(!record.exists());
        })
        .await
        .unwrap();
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields, rename_all = "camelCase")]
    struct TypeScriptOptions {
        #[expect(dead_code, reason = "presence is what this test pins")]
        disable_automatic_typing_acquisition: bool,
        plugins: Vec<serde::de::IgnoredAny>,
        #[expect(dead_code, reason = "presence is what this test pins")]
        tsserver: serde::de::IgnoredAny,
    }

    /// typescript-language-server turns `plugins` into tsserver's
    /// `--globalPlugins`/`--pluginProbeLocations`: conservative mode sends
    /// an explicitly empty list and no other plugin setting.
    #[tokio::test]
    async fn typescript_receives_no_plugin_locations() {
        within(Duration::from_secs(45), async {
            let state = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            let root = repo.path().canonicalize().unwrap();
            std::fs::write(root.join("fixture.rs"), "export function target() {}\n").unwrap();
            let engine = state.path().join("typescript/lib/tsserver.js");
            std::fs::create_dir_all(engine.parent().unwrap()).unwrap();
            std::fs::write(&engine, "// trusted engine stand-in\n").unwrap();
            let mut config = config(state.path());
            config.registry.typescript_server = Some(engine);
            let record = state.path().join("launch");
            let run = run(
                config,
                "normal",
                &record,
                &root,
                lsp::Language::TypeScript,
                "fixture.rs",
            )
            .await;
            assert!(run.outcome.is_ok(), "{:?}", run.outcome);
            let options: TypeScriptOptions = serde_json::from_str(&run.record["init"]).unwrap();
            assert!(options.plugins.is_empty());
        })
        .await
        .unwrap();
    }

    /// pyright reads its interpreter from the `python` section; an operator
    /// pin is answered there. Without one, conservative Python refuses on
    /// Windows (a bare `python` lookup can pick a repository binary there)
    /// and uses the trusted PATH elsewhere.
    #[tokio::test]
    async fn python_gets_the_pinned_interpreter() {
        within(Duration::from_secs(45), async {
            let state = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            let root = repo.path().canonicalize().unwrap();
            std::fs::write(root.join("fixture.rs"), "def target(): pass\n").unwrap();
            let interpreter = state.path().join("python/bin/python3");
            std::fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
            std::fs::write(&interpreter, "not a real interpreter\n").unwrap();
            let mut pinned = config(state.path());
            pinned.registry.python_interpreter = Some(interpreter.clone());
            let record = state.path().join("pinned");
            let first = run(
                pinned,
                "python-config",
                &record,
                &root,
                lsp::Language::Python,
                "fixture.rs",
            )
            .await;
            assert!(first.outcome.is_ok(), "{:?}", first.outcome);
            let sections: Vec<PythonSection> =
                serde_json::from_str(&first.record["python"]).unwrap();
            assert_eq!(sections.len(), 1);
            assert!(same(
                sections
                    .first()
                    .map(|section| section.python_path.to_string_lossy().into_owned())
                    .as_ref(),
                &interpreter
            ));
            let record = state.path().join("unpinned");
            let second = run(
                config(state.path()),
                "normal",
                &record,
                &root,
                lsp::Language::Python,
                "fixture.rs",
            )
            .await;
            if cfg!(windows) {
                assert!(
                    matches!(second.outcome, Err(LspError::PythonInterpreter)),
                    "{:?}",
                    second.outcome
                );
                assert!(!record.exists());
            } else {
                assert!(second.outcome.is_ok(), "{:?}", second.outcome);
            }
        })
        .await
        .unwrap();
    }
}
