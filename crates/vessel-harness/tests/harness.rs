//! Discovery and launch against testkit fakes installed under harness names.

use std::path::PathBuf;

use vessel_harness::fakes::{Role, install};
use vessel_harness::{HarnessError, Lookup, Profile, discover};
use vessel_proc::CommandSpec;
use vessel_proto::{
    HarnessKind, ModelId, ResolvedMcpServers, SessionMode, SessionSpec, StructuredSupport,
    TerminalSize,
};

use support::{TempDir, fake_tui};

#[cfg(test)]
mod support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use vessel_testkit::Fake;

    /// A fresh directory under the system temp dir, removed on drop.
    pub(super) struct TempDir(PathBuf);

    impl TempDir {
        pub(super) fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("vessel-harness-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        pub(super) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    pub(super) fn fake_tui() -> PathBuf {
        Fake::Tui.path().unwrap()
    }
}

/// The launch from before profiles: the default profile, no MCP servers.
#[expect(clippy::panic_in_result_fn, reason = "a test helper")]
fn launch(lookup: &Lookup, spec: &SessionSpec) -> Result<CommandSpec, HarnessError> {
    let profile = Profile::default_for(spec.harness);
    let launch = vessel_harness::launch(lookup, spec, &profile, &ResolvedMcpServers::new(), None)?;
    assert!(launch.mcp_file.is_none() && launch.unattached.is_empty());
    Ok(launch.command)
}

fn terminal(harness: HarnessKind, args: &[&str], cwd: Option<PathBuf>) -> SessionSpec {
    {
        let mut spec = SessionSpec::new(
            harness,
            SessionMode::Terminal {
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                size: TerminalSize::default(),
            },
        );
        spec.cwd = cwd;
        spec
    }
}

fn structured(harness: HarnessKind, cwd: Option<PathBuf>) -> SessionSpec {
    {
        let mut spec = SessionSpec::new(
            harness,
            SessionMode::Structured {
                model: Some(ModelId::new("cheap")),
                resume: None,
            },
        );
        spec.cwd = cwd;
        spec
    }
}

#[tokio::test]
async fn discovers_installed_harnesses_with_versions() {
    let bin = TempDir::new();
    let claude = install(
        bin.path(),
        HarnessKind::ClaudeCode,
        Role::Terminal,
        &fake_tui(),
    )
    .unwrap();
    drop(
        install(
            bin.path(),
            HarnessKind::OpenCode,
            Role::Terminal,
            &fake_tui(),
        )
        .unwrap(),
    );

    let found = discover(&Lookup::new([bin.path().to_path_buf()])).await;

    let kinds: Vec<_> = found.iter().map(|harness| harness.kind).collect();
    assert_eq!(kinds, [HarnessKind::ClaudeCode, HarnessKind::OpenCode]);
    let first = &found[0];
    assert_eq!(first.path, claude);
    assert_eq!(
        first.version.as_deref(),
        Some(format!("fake-tui {}", env!("CARGO_PKG_VERSION")).as_str())
    );
    let StructuredSupport::Unavailable { reason } = &first.structured else {
        panic!("claude without its adapter must not offer structured mode");
    };
    assert!(reason.contains("claude-agent-acp"), "{reason}");
    assert_eq!(
        found[1].structured,
        StructuredSupport::Available,
        "opencode speaks ACP itself"
    );
}

#[tokio::test]
async fn an_installed_adapter_enables_structured_mode() {
    let bin = TempDir::new();
    drop(install(bin.path(), HarnessKind::Codex, Role::Terminal, &fake_tui()).unwrap());
    drop(
        install(
            bin.path(),
            HarnessKind::Codex,
            Role::Structured,
            &fake_tui(),
        )
        .unwrap(),
    );

    let found = discover(&Lookup::new([bin.path().to_path_buf()])).await;

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].structured, StructuredSupport::Available);
}

#[tokio::test]
async fn nothing_installed_means_nothing_found() {
    let bin = TempDir::new();
    assert!(
        discover(&Lookup::new([bin.path().to_path_buf()]))
            .await
            .is_empty()
    );
}

#[test]
fn terminal_launch_passes_arguments_and_cwd() {
    let bin = TempDir::new();
    let work = TempDir::new();
    let claude = install(
        bin.path(),
        HarnessKind::ClaudeCode,
        Role::Terminal,
        &fake_tui(),
    )
    .unwrap();
    let lookup = Lookup::new([bin.path().to_path_buf()]);

    let spec = terminal(
        HarnessKind::ClaudeCode,
        &["--model", "haiku"],
        Some(work.path().to_path_buf()),
    );
    let command = launch(&lookup, &spec).unwrap();

    assert_eq!(command.program, claude);
    assert_eq!(command.args, ["--model", "haiku"]);
    assert_eq!(command.cwd.as_deref(), Some(work.path()));
}

#[test]
fn working_directory_defaults_to_home() {
    let bin = TempDir::new();
    drop(install(bin.path(), HarnessKind::Codex, Role::Terminal, &fake_tui()).unwrap());
    let command = launch(
        &Lookup::new([bin.path().to_path_buf()]),
        &terminal(HarnessKind::Codex, &[], None),
    )
    .unwrap();
    assert_eq!(command.cwd, dirs::home_dir());
}

#[test]
fn native_acp_harnesses_get_their_acp_arguments() {
    let bin = TempDir::new();
    let gemini = install(
        bin.path(),
        HarnessKind::Gemini,
        Role::Structured,
        &fake_tui(),
    )
    .unwrap();
    let opencode = install(
        bin.path(),
        HarnessKind::OpenCode,
        Role::Structured,
        &fake_tui(),
    )
    .unwrap();
    let lookup = Lookup::new([bin.path().to_path_buf()]);

    let command = launch(&lookup, &structured(HarnessKind::Gemini, None)).unwrap();
    assert_eq!(command.program, gemini);
    assert_eq!(command.args, ["--experimental-acp"]);

    let command = launch(&lookup, &structured(HarnessKind::OpenCode, None)).unwrap();
    assert_eq!(command.program, opencode);
    assert_eq!(command.args, ["acp"]);
}

#[test]
fn adapter_harnesses_launch_the_adapter() {
    let bin = TempDir::new();
    let lookup = Lookup::new([bin.path().to_path_buf()]);

    let missing = launch(&lookup, &structured(HarnessKind::ClaudeCode, None)).unwrap_err();
    assert!(
        matches!(&missing, HarnessError::StructuredUnavailable { harness: HarnessKind::ClaudeCode, reason } if reason.contains("npm install")),
        "{missing:?}"
    );

    let adapter = install(
        bin.path(),
        HarnessKind::ClaudeCode,
        Role::Structured,
        &fake_tui(),
    )
    .unwrap();
    let command = launch(&lookup, &structured(HarnessKind::ClaudeCode, None)).unwrap();
    assert_eq!(command.program, adapter);
    assert!(command.args.is_empty());
}

#[test]
fn missing_harness_and_bad_directory_are_errors() {
    let bin = TempDir::new();
    let lookup = Lookup::new([bin.path().to_path_buf()]);
    assert_eq!(
        launch(&lookup, &terminal(HarnessKind::Gemini, &[], None)).unwrap_err(),
        HarnessError::NotInstalled {
            harness: HarnessKind::Gemini,
            binary: "gemini",
        }
    );

    drop(install(bin.path(), HarnessKind::Gemini, Role::Terminal, &fake_tui()).unwrap());
    let nowhere = bin.path().join("no-such-dir");
    assert_eq!(
        launch(
            &lookup,
            &terminal(HarnessKind::Gemini, &[], Some(nowhere.clone()))
        )
        .unwrap_err(),
        HarnessError::NoSuchDirectory { cwd: nowhere }
    );
}

#[test]
fn relative_search_entries_are_ignored() {
    let lookup = Lookup::new([PathBuf::from("relative/bin"), std::env::temp_dir()]);
    assert_eq!(lookup.dirs(), [std::env::temp_dir()]);
}

/// A directory `PATH` syntax cannot express (Unix: a `:` in its name;
/// Windows: a `"`) cannot be searched; it is dropped, and the other
/// directories still find their harnesses.
#[test]
fn a_directory_path_syntax_cannot_express_does_not_hide_the_others() {
    let bin = TempDir::new();
    let claude = install(
        bin.path(),
        HarnessKind::ClaudeCode,
        Role::Terminal,
        &fake_tui(),
    )
    .unwrap();
    let unsayable = if cfg!(windows) { "\"" } else { ":" };
    let odd = bin.path().join(format!("odd{unsayable}name"));
    let lookup = Lookup::new([odd, bin.path().to_path_buf()]);
    assert_eq!(lookup.dirs(), [bin.path().to_path_buf()]);
    let command = launch(
        &lookup,
        &terminal(HarnessKind::ClaudeCode, &[], Some(bin.path().to_path_buf())),
    )
    .unwrap();
    assert_eq!(command.program, claude);
}

#[test]
fn relative_working_directories_are_refused() {
    let bin = TempDir::new();
    drop(install(bin.path(), HarnessKind::Codex, Role::Terminal, &fake_tui()).unwrap());
    let lookup = Lookup::new([bin.path().to_path_buf()]);
    // "." exists relative to the node's own directory; it is still refused,
    // because the client cannot know what it names.
    for relative in [".", "work", "../elsewhere"] {
        let error = launch(
            &lookup,
            &terminal(HarnessKind::Codex, &[], Some(PathBuf::from(relative))),
        )
        .unwrap_err();
        assert_eq!(
            error,
            HarnessError::RelativeDirectory {
                cwd: PathBuf::from(relative)
            }
        );
    }
}
