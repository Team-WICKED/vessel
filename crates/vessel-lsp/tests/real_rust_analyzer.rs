//! Conservative-mode regressions against a real rust-analyzer and rustup.
//!
//! Ignored by the ordinary gate: they need rust-analyzer, rustc and cargo
//! (normally rustup proxies) on PATH. Run them with
//! `cargo test -p vessel-lsp --test real_rust_analyzer -- --ignored --test-threads=1`.
//! `VESSEL_TEST_RUST_ANALYZER` may name the server binary directly, for a
//! host whose default toolchain lacks the rust-analyzer component.
//! A fixture's "repository" toolchain only appends to a marker file and then
//! runs the real tool, so a regression shows up as a non-empty marker.

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use vessel_lsp::{Config, LspError, Manager, Query, Scope, registry::Override};
    use vessel_proto::{Reply, lsp};

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn executable(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        write(path, text);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The real toolchain binaries, asked for from a directory with no
    /// toolchain files so a repository cannot influence the answer.
    fn real_tool(neutral: &Path, name: &str) -> PathBuf {
        let output = std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .current_dir(neutral)
            .output()
            .expect("rustc must be on PATH");
        assert!(output.status.success());
        let sysroot = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        sysroot.join("bin").join(name)
    }

    /// An operator-like node: the process PATH (with its rustup proxies),
    /// an isolated home that links only the installed toolchains, the real
    /// rust-analyzer, and a node-owned state directory. Conservative.
    fn operator(base: &Path) -> Config {
        let home = base.join("home");
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        let user = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let rustup =
            std::env::var_os("RUSTUP_HOME").map_or_else(|| user.join(".rustup"), PathBuf::from);
        let cargo =
            std::env::var_os("CARGO_HOME").map_or_else(|| user.join(".cargo"), PathBuf::from);
        std::os::unix::fs::symlink(rustup, home.join(".rustup")).unwrap();
        std::os::unix::fs::symlink(cargo.join("bin"), home.join(".cargo/bin")).unwrap();
        let mut config = Config {
            request_timeout: Duration::from_secs(120),
            ..Config::default()
        };
        config.registry.search_path = std::env::var_os("PATH").unwrap_or_default();
        config.registry.home = Some(home);
        config.registry.state_dir = Some(base.join("state"));
        let server = std::env::var_os("VESSEL_TEST_RUST_ANALYZER").map_or_else(
            || which::which("rust-analyzer").expect("rust-analyzer must be on PATH"),
            PathBuf::from,
        );
        let _previous = config.registry.overrides.insert(
            lsp::Language::Rust,
            Override {
                program: server,
                args: Vec::new(),
            },
        );
        config
    }

    fn symbols(root: &Path, file: &str) -> Query {
        Query::DocumentSymbols(lsp::DocumentSymbols {
            workspace: lsp::Workspace {
                root: root.to_owned(),
                language: lsp::Language::Rust,
            },
            file: lsp::FilePath::try_from(PathBuf::from(file)).unwrap(),
            page: lsp::PageRequest::default(),
        })
    }

    async fn run(config: Config, root: &Path, query: Query) -> Result<Reply, LspError> {
        let stop = CancellationToken::new();
        let manager = Manager::start(config, stop.child_token());
        // The server answers only after its initial load reached quiescence,
        // so every toolchain probe of that load has finished by now.
        let scope = Scope::new(root.to_owned(), root.to_owned(), false).unwrap();
        let outcome = manager.query(scope, query).await;
        stop.cancel();
        manager.stopped().await;
        outcome
    }

    /// A root whose Rust lives one directory down, as in a monorepo.
    fn monorepo(base: &Path) -> (PathBuf, PathBuf) {
        let root = base.join("repo");
        write(&root.join("README.md"), "Rust lives in sub/\n");
        write(
            &root.join("sub/Cargo.toml"),
            "[package]\nname = \"sub\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            &root.join("sub/src/lib.rs"),
            "pub fn hello() -> u32 { 1 }\n",
        );
        (root.canonicalize().unwrap(), base.join("marker"))
    }

    /// Review finding (high): rust-analyzer found `sub/Cargo.toml` itself and
    /// ran the rustup proxy with cwd `sub`, which honoured a `path` toolchain
    /// inside the repository. The policy never looked below the root.
    #[tokio::test]
    #[ignore = "needs rust-analyzer and rustup proxies on PATH"]
    async fn a_child_rust_toolchain_never_runs_a_repository_binary() {
        let base = tempfile::tempdir().unwrap();
        let (root, marker) = monorepo(base.path());
        let toolchain = root.join("sub/evil-toolchain");
        write(
            &root.join("sub/rust-toolchain.toml"),
            &format!("[toolchain]\npath = \"{}\"\n", toolchain.display()),
        );
        for tool in ["rustc", "cargo"] {
            let real = real_tool(base.path(), tool);
            executable(
                &toolchain.join("bin").join(tool),
                &format!(
                    "#!/bin/sh\necho \"repository {tool} ran\" >> '{}'\nexec '{}' \"$@\"\n",
                    marker.display(),
                    real.display()
                ),
            );
        }
        let outcome = run(
            operator(base.path()),
            &root,
            symbols(&root, "sub/src/lib.rs"),
        )
        .await;
        let ran = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(ran.is_empty(), "repository toolchain ran: {ran}");
        assert!(
            matches!(outcome, Err(LspError::ProjectConfiguration)),
            "{outcome:?}"
        );
    }

    /// Kept as a probe: a child `rust-analyzer.toml` asking for build scripts
    /// and an override command must not run anything.
    #[tokio::test]
    #[ignore = "needs rust-analyzer and rustup proxies on PATH"]
    async fn a_child_rust_analyzer_toml_runs_nothing() {
        let base = tempfile::tempdir().unwrap();
        let (root, marker) = monorepo(base.path());
        write(
            &root.join("sub/rust-analyzer.toml"),
            &format!(
                "[cargo.buildScripts]\nenable = true\noverrideCommand = [\"/bin/sh\", \"-c\", \"echo override ran >> '{}'; echo\"]\n\n[procMacro]\nenable = true\n",
                marker.display()
            ),
        );
        let outcome = run(
            operator(base.path()),
            &root,
            symbols(&root, "sub/src/lib.rs"),
        )
        .await;
        let ran = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(ran.is_empty(), "repository command ran: {ran}");
        assert!(
            matches!(outcome, Err(LspError::ProjectConfiguration)),
            "{outcome:?}"
        );
    }

    /// The safe layout still works: the checked child project is linked
    /// explicitly and analysed semantically.
    #[tokio::test]
    #[ignore = "needs rust-analyzer and rustup proxies on PATH"]
    async fn a_checked_child_project_is_linked_and_analysed() {
        let base = tempfile::tempdir().unwrap();
        let (root, _marker) = monorepo(base.path());
        let query = Query::WorkspaceSymbols(lsp::WorkspaceSymbols {
            workspace: lsp::Workspace {
                root: root.clone(),
                language: lsp::Language::Rust,
            },
            query: "hello".into(),
            page: lsp::PageRequest::default(),
        });
        let outcome = run(operator(base.path()), &root, query).await;
        let Ok(Reply::LspWorkspaceSymbols(page)) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(
            page.items.iter().any(|symbol| symbol.name == "hello"),
            "{page:?}"
        );
    }

    /// Repository layouts from the verification round. Each one plants
    /// code that would mark the marker file if it ran.
    #[derive(Debug, Clone, Copy)]
    enum Layout {
        /// `sub/rust-toolchain` (legacy name) with a path toolchain.
        LegacyToolchain,
        /// A path toolchain in a workspace member two levels down.
        DeepToolchain,
        /// `sub/.cargo/config` (legacy name) forcing RUSTC.
        LegacyCargo,
        /// A `.cargo/config` above the root forcing RUSTC.
        ParentCargo,
        /// `sub` is a symlink to a project outside the root.
        SymlinkProject,
        /// A root workspace whose sibling member carries traps.
        OutsideMember,
        /// `package.workspace` pointing sideways to a trapped workspace.
        SidewaysWorkspace,
        /// A member's `rust-analyzer.toml` asking for build scripts.
        MemberAnalyzerToml,
        /// A nested workspace dependency with `rust-analyzer.toml`.
        NestedWorkspaceAnalyzerToml,
    }

    impl Layout {
        const ALL: [Self; 9] = [
            Self::LegacyToolchain,
            Self::DeepToolchain,
            Self::LegacyCargo,
            Self::ParentCargo,
            Self::SymlinkProject,
            Self::OutsideMember,
            Self::SidewaysWorkspace,
            Self::MemberAnalyzerToml,
            Self::NestedWorkspaceAnalyzerToml,
        ];

        /// Plant the layout; returns the file to query.
        fn plant(self, base: &Path, root: &Path, marker: &Path) -> &'static str {
            let toolchain = root.join("evil-toolchain");
            for tool in ["rustc", "cargo"] {
                executable(
                    &toolchain.join("bin").join(tool),
                    &format!(
                        "#!/bin/sh\necho ran >> '{}'\nexec '{}' \"$@\"\n",
                        marker.display(),
                        real_tool(base, tool).display()
                    ),
                );
            }
            let path_toolchain = format!("[toolchain]\npath='{}'\n", toolchain.display());
            let forced_rustc = "[env]\nRUSTC={value='./evil',force=true}\n";
            let package =
                |name: &str| format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n");
            match self {
                Self::LegacyToolchain => write(&root.join("sub/rust-toolchain"), &path_toolchain),
                Self::DeepToolchain => {
                    member(root, &package("member"));
                    write(
                        &root.join("sub/Cargo.toml"),
                        "[workspace]\nmembers=['member']\nresolver='2'\n",
                    );
                    write(
                        &root.join("sub/member/rust-toolchain.toml"),
                        &path_toolchain,
                    );
                    return "sub/member/src/lib.rs";
                }
                Self::LegacyCargo => write(&root.join("sub/.cargo/config"), forced_rustc),
                Self::ParentCargo => write(&base.join(".cargo/config"), forced_rustc),
                Self::SymlinkProject => {
                    std::fs::rename(root.join("sub"), base.join("actual")).unwrap();
                    std::os::unix::fs::symlink(base.join("actual"), root.join("sub")).unwrap();
                }
                Self::OutsideMember => {
                    write(
                        &root.join("Cargo.toml"),
                        "[workspace]\nmembers=['sub','sibling']\nresolver='2'\n",
                    );
                    write(&root.join("sibling/Cargo.toml"), &package("sibling"));
                    write(&root.join("sibling/src/lib.rs"), "pub fn sibling() {}\n");
                    write(&root.join("sibling/rust-toolchain"), &path_toolchain);
                    write(&root.join("sibling/.cargo/config"), forced_rustc);
                }
                Self::SidewaysWorkspace => {
                    // Cargo's explicit workspace pointer is not an ancestor.
                    write(
                        &root.join("sub/Cargo.toml"),
                        &format!("{}workspace='../hidden/ws'\n", package("sub")),
                    );
                    write(
                        &root.join("hidden/ws/Cargo.toml"),
                        &format!(
                            "[workspace]\nmembers=['../../sub']\nresolver='2'\n{}",
                            package("ws")
                        ),
                    );
                    write(&root.join("hidden/ws/src/lib.rs"), "pub fn ws() {}\n");
                    write(&root.join("hidden/ws/rust-analyzer.toml"), &trap(marker));
                    write(
                        &root.join("hidden/ws/.cargo/config"),
                        &format!(
                            "[env]\nRUSTC={{value='{}',force=true}}\n",
                            toolchain.join("bin/rustc").display()
                        ),
                    );
                }
                Self::MemberAnalyzerToml => {
                    member(root, &package("member"));
                    write(
                        &root.join("sub/Cargo.toml"),
                        "[workspace]\nmembers=['member']\nresolver='2'\n",
                    );
                    write(&root.join("sub/member/build.rs"), &build_script(marker));
                    write(&root.join("sub/member/rust-analyzer.toml"), &trap(marker));
                    return "sub/member/src/lib.rs";
                }
                Self::NestedWorkspaceAnalyzerToml => {
                    member(root, &format!("{}[workspace]\n", package("member")));
                    write(
                        &root.join("sub/Cargo.toml"),
                        &format!(
                            "{}[dependencies]\nmember={{path='member'}}\n",
                            package("sub")
                        ),
                    );
                    write(&root.join("sub/member/build.rs"), &build_script(marker));
                    write(&root.join("sub/member/rust-analyzer.toml"), &trap(marker));
                    return "sub/member/src/lib.rs";
                }
            }
            "sub/src/lib.rs"
        }
    }

    fn member(root: &Path, manifest: &str) {
        write(&root.join("sub/member/Cargo.toml"), manifest);
        write(&root.join("sub/member/src/lib.rs"), "pub fn member() {}\n");
    }

    fn trap(marker: &Path) -> String {
        format!(
            "[cargo.buildScripts]\nenable=true\noverrideCommand=[\"/bin/sh\",\"-c\",\"echo ran >> {}; echo\"]\n[procMacro]\nenable=true\n",
            marker.display()
        )
    }

    fn build_script(marker: &Path) -> String {
        format!("fn main() {{ std::fs::write({marker:?}, \"build ran\").unwrap(); }}\n")
    }

    /// The verification round's layout matrix: no layout runs repository
    /// code, refusals are the expected ones, and the rest still analyse.
    #[tokio::test]
    #[ignore = "needs rust-analyzer and rustup proxies on PATH"]
    async fn review_rust_layout_variations() {
        for layout in Layout::ALL {
            let base = tempfile::tempdir().unwrap();
            let (root, marker) = monorepo(base.path());
            let file = layout.plant(base.path(), &root, &marker);
            let query = match layout {
                // Workspace symbols do not fail file confinement first.
                Layout::SymlinkProject => Query::WorkspaceSymbols(lsp::WorkspaceSymbols {
                    workspace: lsp::Workspace {
                        root: root.clone(),
                        language: lsp::Language::Rust,
                    },
                    query: "hello".into(),
                    page: lsp::PageRequest::default(),
                }),
                Layout::LegacyToolchain
                | Layout::DeepToolchain
                | Layout::LegacyCargo
                | Layout::ParentCargo
                | Layout::OutsideMember
                | Layout::SidewaysWorkspace
                | Layout::MemberAnalyzerToml
                | Layout::NestedWorkspaceAnalyzerToml => symbols(&root, file),
            };
            let outcome = run(operator(base.path()), &root, query).await;
            let ran = std::fs::read_to_string(&marker).unwrap_or_default();
            assert!(ran.is_empty(), "{layout:?}: repository code ran: {ran}");
            match layout {
                Layout::LegacyToolchain | Layout::LegacyCargo | Layout::ParentCargo => assert!(
                    matches!(outcome, Err(LspError::ProjectConfiguration)),
                    "{layout:?}: {outcome:?}"
                ),
                Layout::SymlinkProject => {
                    assert!(matches!(outcome, Err(LspError::NoProject)), "{outcome:?}");
                }
                Layout::DeepToolchain
                | Layout::OutsideMember
                | Layout::SidewaysWorkspace
                | Layout::MemberAnalyzerToml
                | Layout::NestedWorkspaceAnalyzerToml => {
                    assert!(outcome.is_ok(), "{layout:?}: {outcome:?}");
                }
            }
        }
    }

    /// Explicit path toolchains need the same all-configured-roots boundary
    /// as linked defaults; otherwise another served repository owns the tools.
    #[tokio::test]
    #[ignore = "needs rust-analyzer and rustup proxies on PATH"]
    async fn review_explicit_path_toolchain_cannot_live_in_another_root() {
        let base = tempfile::tempdir().unwrap();
        let (root, marker) = monorepo(base.path());
        let other = base.path().join("other-repository");
        let tools = other.join("toolchain");
        for tool in ["rustc", "cargo"] {
            executable(
                &tools.join("bin").join(tool),
                &format!(
                    "#!/bin/sh\necho '{tool} ran' >> '{}'\nexec '{}' \"$@\"\n",
                    marker.display(),
                    real_tool(base.path(), tool).display()
                ),
            );
        }
        let mut config = operator(base.path());
        config.registry.rust_toolchain = Some(tools);
        config.registry.configured_roots = vec![root.clone(), other];
        let outcome = run(config, &root, symbols(&root, "sub/src/lib.rs")).await;
        let ran = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(ran.is_empty(), "configured-root toolchain executed: {ran}");
        assert!(
            matches!(outcome, Err(LspError::ProjectConfiguration)),
            "{outcome:?}"
        );
    }
}
