//! Repository traps exercise the actual launch boundary, using only fakes.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use vessel_lsp::{Config, LspError, Manager, Query, registry::Override};
    use vessel_proto::lsp;
    use vessel_testkit::{Fake, within};

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
                let path = root.path().join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, contents).unwrap();
                let marker = root.path().join("server-started");
                let mut config = Config::default();
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
                let mut config = Config::default();
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
