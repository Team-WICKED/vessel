//! Launching with profiles and MCP servers, against testkit fakes installed
//! under harness names. No real harness runs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use vessel_harness::fakes::{Role, install};
use vessel_harness::{HarnessError, Launch, Lookup, McpError, Profile, login_check};
use vessel_proc::EnvValue;
use vessel_proto::{HarnessKind, McpTransport, ResolvedMcpServers, ResolvedValue, SessionSpec};

use support::{
    HTTP_SECRET, STDIO_SECRET, TempDir, asking, assert_no_secret_in_debug,
    assert_no_secret_on_argv, fake, profile, servers, set, structured, terminal, under_home,
};

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
                .join(format!("vessel-profiles-test-{}-{n}", std::process::id()));
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

    pub(super) fn fake(which: Fake) -> PathBuf {
        which.path().unwrap()
    }

    use std::collections::BTreeMap;

    use vessel_harness::{Launch, Profile, parse_profiles};
    use vessel_proc::{CommandSpec, EnvValue};
    use vessel_proto::{
        HarnessKind, McpTransport, ResolvedMcpServers, ResolvedValue, SessionMode, SessionSpec,
        TerminalSize,
    };

    pub(super) const STDIO_SECRET: &str = "sk-stdio-secret-1f2e";
    pub(super) const HTTP_SECRET: &str = "Bearer sk-http-secret-9a8b";
    pub(super) const PROFILE_SECRET: &str = "sk-profile-secret-77";

    pub(super) fn servers() -> ResolvedMcpServers {
        BTreeMap::from([
            (
                "docs".parse().unwrap(),
                McpTransport::Stdio {
                    command: PathBuf::from("/opt/docs-mcp"),
                    args: vec!["--stdio".to_owned()],
                    env: BTreeMap::from([(
                        "DOCS_TOKEN".parse().unwrap(),
                        ResolvedValue::new(STDIO_SECRET.to_owned()),
                    )]),
                },
            ),
            (
                "web".parse().unwrap(),
                McpTransport::Http {
                    url: "https://mcp.example.invalid/mcp".parse().unwrap(),
                    headers: BTreeMap::from([(
                        "Authorization".parse().unwrap(),
                        ResolvedValue::new(HTTP_SECRET.to_owned()),
                    )]),
                },
            ),
        ])
    }

    /// No value of any server, nor the profile's secret, on the command line.
    pub(super) fn assert_no_secret_on_argv(command: &CommandSpec) {
        let mut line = vec![command.program.as_os_str().to_string_lossy().into_owned()];
        line.extend(
            command
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned()),
        );
        for secret in [STDIO_SECRET, HTTP_SECRET, "sk-http-secret", PROFILE_SECRET] {
            assert!(
                line.iter().all(|arg| !arg.contains(secret)),
                "{secret} on the command line: {line:?}"
            );
        }
    }

    pub(super) fn assert_no_secret_in_debug(launch: &Launch) {
        let shown = format!("{launch:?}");
        for secret in [STDIO_SECRET, "sk-http-secret", PROFILE_SECRET] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    pub(super) fn terminal(harness: HarnessKind, args: &[&str], cwd: &Path) -> SessionSpec {
        SessionSpec::new(
            harness,
            SessionMode::Terminal {
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                size: TerminalSize::default(),
            },
        )
        .with_cwd(cwd)
    }

    pub(super) fn structured(harness: HarnessKind, cwd: &Path) -> SessionSpec {
        SessionSpec::new(
            harness,
            SessionMode::Structured {
                model: None,
                resume: None,
            },
        )
        .with_cwd(cwd)
    }

    /// `spec` asking for `profile` by name, as the node passes it.
    pub(super) fn asking(spec: SessionSpec, profile: &Profile) -> SessionSpec {
        spec.with_profile(profile.name().clone())
    }

    pub(super) const PROFILES: &str = r#"
    [profiles.claude-alt]
    harness = "claude"
    env = { CLAUDE_CONFIG_DIR = "~/.claude-alt", ANTHROPIC_AUTH_TOKEN = "sk-profile-secret-77" }
    unset = ["ANTHROPIC_API_KEY"]
    args = ["--model", "opus"]

    [profiles.codex-work]
    harness = "codex"
    env = { CODEX_HOME = "~/.codex-work" }

    [profiles.codex-shares]
    harness = "codex"
    env = { DOCS_TOKEN = "another value" }

    [profiles.gemini-plain]
    harness = "gemini"
    args = ["--yolo"]
    "#;

    /// `name` under the profiles' home, as `~/name` expands: the system's
    /// separator.
    pub(super) fn under_home(name: &str) -> String {
        format!("/home/u{}{name}", std::path::MAIN_SEPARATOR)
    }

    pub(super) fn profile(name: &str) -> Profile {
        parse_profiles(PROFILES, Some(Path::new("/home/u")))
            .unwrap()
            .get(&name.parse().unwrap())
            .cloned()
            .unwrap()
    }

    pub(super) fn set(command: &CommandSpec, name: &str) -> Option<String> {
        match command.env_change(name) {
            Some(EnvValue::Set(value)) => Some(value.to_string_lossy().into_owned()),
            Some(EnvValue::Remove) | None => None,
        }
    }
}

/// Launch `spec` asking for `profile` by name, as the node does, with the
/// MCP file (if any) in the test's own directory (the lookup's).
fn launch(
    lookup: &Lookup,
    spec: &SessionSpec,
    profile: &Profile,
    mcp: &ResolvedMcpServers,
) -> Result<Launch, HarnessError> {
    let dir = lookup.dirs().first().map(PathBuf::as_path);
    vessel_harness::launch(lookup, &asking(spec.clone(), profile), profile, mcp, dir)
}

#[test]
fn a_claude_terminal_gets_the_profile_and_an_owner_only_mcp_file() {
    let bin = TempDir::new();
    let tui = fake(vessel_testkit::Fake::Tui);
    let _claude = install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);

    let launched = launch(
        &lookup,
        &terminal(HarnessKind::ClaudeCode, &["hello"], bin.path()),
        &profile("claude-alt"),
        &servers(),
    )
    .unwrap();
    let command = &launched.command;
    assert_no_secret_on_argv(command);
    assert_no_secret_in_debug(&launched);
    assert!(launched.unattached.is_empty());

    // MCP first, as one argument, then the profile's, then the session's.
    let file = launched.mcp_file.as_ref().unwrap();
    let mut flag = std::ffi::OsString::from("--mcp-config=");
    flag.push(file.path());
    assert_eq!(
        command.args,
        [flag, "--model".into(), "opus".into(), "hello".into()]
    );
    assert_eq!(
        set(command, "CLAUDE_CONFIG_DIR").as_deref(),
        Some(under_home(".claude-alt").as_str())
    );
    assert_eq!(
        command.env_change("ANTHROPIC_API_KEY"),
        Some(&EnvValue::Remove)
    );

    // The file is written only when asked, exclusively and owner-only, and
    // it holds the values; dropping the guard removes it.
    assert!(file.path().starts_with(bin.path()));
    assert!(!file.path().exists());
    let Launch { mcp_file, .. } = launched;
    let written = mcp_file.unwrap().write().unwrap();
    let text = std::fs::read_to_string(written.path()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let docs = &json["mcpServers"]["docs"];
    assert_eq!(docs["type"], "stdio");
    assert_eq!(docs["command"], "/opt/docs-mcp");
    assert_eq!(docs["args"][0], "--stdio");
    assert_eq!(docs["env"]["DOCS_TOKEN"], STDIO_SECRET);
    let web = &json["mcpServers"]["web"];
    assert_eq!(web["type"], "http");
    assert_eq!(web["url"], "https://mcp.example.invalid/mcp");
    assert_eq!(web["headers"]["Authorization"], HTTP_SECRET);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(written.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let path = written.path().to_owned();
    drop(written);
    assert!(!path.exists());
}

#[test]
fn an_mcp_file_never_replaces_an_existing_one() {
    let bin = TempDir::new();
    let tui = fake(vessel_testkit::Fake::Tui);
    let _claude = install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);
    let launched = launch(
        &lookup,
        &terminal(HarnessKind::ClaudeCode, &[], bin.path()),
        &Profile::default_for(HarnessKind::ClaudeCode),
        &servers(),
    )
    .unwrap();
    let file = launched.mcp_file.unwrap();
    std::fs::write(file.path(), "planted").unwrap();
    let path = file.path().to_owned();
    assert!(file.write().is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "planted");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn codex_gets_servers_as_overrides_and_values_in_its_environment() {
    let bin = TempDir::new();
    let tui = fake(vessel_testkit::Fake::Tui);
    let _codex = install(bin.path(), HarnessKind::Codex, Role::Terminal, &tui).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);

    let launched = launch(
        &lookup,
        &terminal(HarnessKind::Codex, &["resume"], bin.path()),
        &profile("codex-work"),
        &servers(),
    )
    .unwrap();
    let command = &launched.command;
    assert_no_secret_on_argv(command);
    assert_no_secret_in_debug(&launched);
    assert!(launched.mcp_file.is_none());
    assert!(launched.unattached.is_empty());
    assert_eq!(
        set(command, "CODEX_HOME").as_deref(),
        Some(under_home(".codex-work").as_str())
    );
    assert_eq!(set(command, "DOCS_TOKEN").as_deref(), Some(STDIO_SECRET));
    assert_eq!(
        set(command, "VESSEL_MCP_1_0_SECRET").as_deref(),
        Some(HTTP_SECRET)
    );

    let args: Vec<String> = command
        .args
        .iter()
        .map(|arg| arg.to_str().unwrap().to_owned())
        .collect();
    assert_eq!(args.len(), 5, "{args:?}");
    assert_eq!(args.first().map(String::as_str), Some("-c"));
    assert_eq!(args.last().map(String::as_str), Some("resume"));
    // Each override is `key=value` with the value TOML, as Codex parses it.
    let mut overrides = BTreeMap::new();
    for pair in args.chunks_exact(2) {
        let [flag, setting] = pair else {
            panic!("pairs");
        };
        assert_eq!(flag, "-c");
        let (key, value) = setting.split_once('=').unwrap();
        let document: toml_edit::DocumentMut = format!("value = {value}").parse().unwrap();
        let _old = overrides.insert(key.to_owned(), document);
    }
    let docs = &overrides["mcp_servers.docs"]["value"];
    assert_eq!(docs["command"].as_str(), Some("/opt/docs-mcp"));
    assert_eq!(docs["args"].as_array().unwrap().len(), 1);
    let forwarded: Vec<_> = docs["env_vars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    assert_eq!(forwarded, ["DOCS_TOKEN"]);
    let web = &overrides["mcp_servers.web"]["value"];
    assert_eq!(web["url"].as_str(), Some("https://mcp.example.invalid/mcp"));
    assert_eq!(
        web["env_http_headers"]["Authorization"].as_str(),
        Some("VESSEL_MCP_1_0_SECRET")
    );
}

#[test]
fn codex_refuses_one_variable_with_two_values() {
    let bin = TempDir::new();
    let tui = fake(vessel_testkit::Fake::Tui);
    let _codex = install(bin.path(), HarnessKind::Codex, Role::Terminal, &tui).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);
    let spec = terminal(HarnessKind::Codex, &[], bin.path());
    let shared = || McpError::SharedVariable {
        name: "DOCS_TOKEN".parse().unwrap(),
    };

    // Two servers, one variable, two values.
    let mut two = servers();
    let _old = two.insert(
        "more".parse().unwrap(),
        McpTransport::Stdio {
            command: PathBuf::from("/opt/more"),
            args: Vec::new(),
            env: BTreeMap::from([(
                "DOCS_TOKEN".parse().unwrap(),
                ResolvedValue::new("different".to_owned()),
            )]),
        },
    );
    let default = Profile::default_for(HarnessKind::Codex);
    assert_eq!(
        format!("{:?}", launch(&lookup, &spec, &default, &two).unwrap_err()),
        format!("{:?}", HarnessError::Mcp(shared()))
    );
    // The profile sets it to something else.
    assert_eq!(
        format!(
            "{:?}",
            launch(&lookup, &spec, &profile("codex-shares"), &servers()).unwrap_err()
        ),
        format!("{:?}", HarnessError::Mcp(shared()))
    );
    // A server may not move Codex's home through Codex's environment.
    let homing = BTreeMap::from([(
        "home".parse().unwrap(),
        McpTransport::Stdio {
            command: PathBuf::from("/opt/x"),
            args: Vec::new(),
            env: BTreeMap::from([(
                "CODEX_HOME".parse().unwrap(),
                ResolvedValue::new("/elsewhere".to_owned()),
            )]),
        },
    )]);
    assert_eq!(
        format!(
            "{:?}",
            launch(&lookup, &spec, &default, &homing).unwrap_err()
        ),
        format!(
            "{:?}",
            HarnessError::Mcp(McpError::ProtectedVariable {
                name: "CODEX_HOME".parse().unwrap()
            })
        )
    );
    // Nor change what Codex inherits from the node (cargo sets this one
    // for the test process).
    let inherited = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let clashing = |value: &str| {
        BTreeMap::from([(
            "dir".parse().unwrap(),
            McpTransport::Stdio {
                command: PathBuf::from("/opt/x"),
                args: Vec::new(),
                env: BTreeMap::from([(
                    "CARGO_MANIFEST_DIR".parse().unwrap(),
                    ResolvedValue::new(value.to_owned()),
                )]),
            },
        )])
    };
    assert_eq!(
        format!(
            "{:?}",
            launch(&lookup, &spec, &default, &clashing("/elsewhere")).unwrap_err()
        ),
        format!(
            "{:?}",
            HarnessError::Mcp(McpError::SharedVariable {
                name: "CARGO_MANIFEST_DIR".parse().unwrap()
            })
        )
    );
    assert!(launch(&lookup, &spec, &default, &clashing(&inherited)).is_ok());
}

#[test]
fn harnesses_without_a_launch_form_report_the_servers_unattached() {
    let bin = TempDir::new();
    let tui = fake(vessel_testkit::Fake::Tui);
    let _gemini = install(bin.path(), HarnessKind::Gemini, Role::Terminal, &tui).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);
    let launched = launch(
        &lookup,
        &terminal(HarnessKind::Gemini, &["-p", "x"], bin.path()),
        &profile("gemini-plain"),
        &servers(),
    )
    .unwrap();
    assert!(launched.mcp_file.is_none());
    let names: Vec<_> = launched
        .unattached
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(names, ["docs", "web"]);
    assert_eq!(launched.command.args, ["--yolo", "-p", "x"]);
    assert!(launched.command.env.is_empty());
}

#[test]
fn structured_sessions_take_the_environment_but_not_arguments_or_servers() {
    let bin = TempDir::new();
    let acp = fake(vessel_testkit::Fake::Acp);
    let _adapter = install(bin.path(), HarnessKind::ClaudeCode, Role::Structured, &acp).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);
    let launched = launch(
        &lookup,
        &structured(HarnessKind::ClaudeCode, bin.path()),
        &profile("claude-alt"),
        &servers(),
    )
    .unwrap();
    assert!(launched.mcp_file.is_none());
    assert!(launched.unattached.is_empty());
    assert!(launched.command.args.is_empty());
    assert_eq!(
        set(&launched.command, "CLAUDE_CONFIG_DIR").as_deref(),
        Some(under_home(".claude-alt").as_str())
    );
    assert_eq!(
        launched.command.env_change("ANTHROPIC_API_KEY"),
        Some(&EnvValue::Remove)
    );
    assert_no_secret_on_argv(&launched.command);
    assert_no_secret_in_debug(&launched);
}

#[test]
fn another_harness_profile_is_refused() {
    let bin = TempDir::new();
    let tui = fake(vessel_testkit::Fake::Tui);
    let _claude = install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);
    let error = launch(
        &lookup,
        &terminal(HarnessKind::ClaudeCode, &[], bin.path()),
        &profile("codex-work"),
        &ResolvedMcpServers::new(),
    )
    .unwrap_err();
    assert_eq!(
        format!("{error:?}"),
        format!(
            "{:?}",
            HarnessError::ProfileMismatch {
                profile: "codex-work".parse().unwrap(),
                profile_harness: HarnessKind::Codex,
                harness: HarnessKind::ClaudeCode,
            }
        )
    );
}

#[test]
fn login_checks_run_with_the_profile() {
    let bin = TempDir::new();
    let tui = fake(vessel_testkit::Fake::Tui);
    let claude = install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap();
    let _codex = install(bin.path(), HarnessKind::Codex, Role::Terminal, &tui).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);

    let check = login_check(&lookup, &profile("claude-alt"))
        .unwrap()
        .unwrap();
    assert_eq!(check.program, claude);
    assert_eq!(check.args, ["auth", "status"]);
    assert_eq!(
        set(&check, "CLAUDE_CONFIG_DIR").as_deref(),
        Some(under_home(".claude-alt").as_str())
    );
    assert_eq!(
        check.env_change("ANTHROPIC_API_KEY"),
        Some(&EnvValue::Remove)
    );

    let check = login_check(&lookup, &profile("codex-work"))
        .unwrap()
        .unwrap();
    assert_eq!(check.args, ["login", "status"]);
    assert!(matches!(
        login_check(&lookup, &profile("gemini-plain")),
        Ok(None)
    ));
}

#[test]
fn the_profile_must_be_the_one_the_spec_asks_for() {
    let bin = TempDir::new();
    let tui = fake(vessel_testkit::Fake::Tui);
    let _claude = install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);
    let none = ResolvedMcpServers::new();
    let spec = terminal(HarnessKind::ClaudeCode, &[], bin.path());
    let alt = profile("claude-alt");
    let default = Profile::default_for(HarnessKind::ClaudeCode);

    // No profile named: only the default will do.
    assert_eq!(
        format!(
            "{:?}",
            vessel_harness::launch(&lookup, &spec, &alt, &none, None).unwrap_err()
        ),
        format!(
            "{:?}",
            HarnessError::ProfileNotRequested {
                requested: None,
                given: alt.name().clone(),
            }
        )
    );
    assert!(vessel_harness::launch(&lookup, &spec, &default, &none, None).is_ok());
    // The default named explicitly is the default.
    let named_default = asking(spec.clone(), &default);
    assert!(vessel_harness::launch(&lookup, &named_default, &default, &none, None).is_ok());
    // A named profile: that one, not the default.
    let wants_alt = asking(spec, &alt);
    assert!(vessel_harness::launch(&lookup, &wants_alt, &alt, &none, None).is_ok());
    assert_eq!(
        format!(
            "{:?}",
            vessel_harness::launch(&lookup, &wants_alt, &default, &none, None).unwrap_err()
        ),
        format!(
            "{:?}",
            HarnessError::ProfileNotRequested {
                requested: Some(alt.name().clone()),
                given: default.name().clone(),
            }
        )
    );
}

/// Review finding 3: no shared fallback for the MCP file.
#[test]
fn a_harness_that_needs_an_mcp_file_needs_a_directory() {
    let bin = TempDir::new();
    let tui = fake(vessel_testkit::Fake::Tui);
    let _claude = install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap();
    let lookup = Lookup::new([bin.path().to_owned()]);
    let spec = terminal(HarnessKind::ClaudeCode, &[], bin.path());
    let default = Profile::default_for(HarnessKind::ClaudeCode);
    assert_eq!(
        format!(
            "{:?}",
            vessel_harness::launch(&lookup, &spec, &default, &servers(), None).unwrap_err()
        ),
        format!("{:?}", HarnessError::NoMcpDirectory)
    );
    // Without servers, or for a harness without a file, none is needed.
    let none = ResolvedMcpServers::new();
    assert!(vessel_harness::launch(&lookup, &spec, &default, &none, None).is_ok());
}
