//! Profiles and MCP servers through the session runtime, with `fake-tui`
//! installed as `claude` and `codex`, and `fake-acp` as `claude-agent-acp`.
//! The fakes report the environment, arguments and MCP servers they got.
//!
//! Tests wait on events, never on time; the timeout only turns a hang into
//! a failure.

use vessel_harness::{HarnessError, Profile, home};
use vessel_proto::{ErrorCode, HarnessKind, HarnessSessionId, SessionStatus};
use vessel_session::{LaunchOptions, SessionError};

use support::{
    Fixture, Reader, SECRETS, inherited, named, profiles, servers, structured, terminal, within,
};

#[cfg(test)]
mod support {
    use std::collections::{BTreeMap, VecDeque};
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::sync::broadcast::error::RecvError;
    use tokio_util::sync::CancellationToken;
    use vessel_harness::fakes::{Role, install};
    use vessel_harness::{Lookup, Profile, Profiles, parse_profiles};
    use vessel_proto::{
        AgentEvent, ContentBlock, EventRecord, EventSeq, HarnessKind, HarnessSessionId,
        McpTransport, ResolvedMcpServers, ResolvedValue, SessionEvent, SessionInput, SessionMode,
        SessionSpec, SessionStatus, TerminalBytes, TerminalInput, TerminalSize,
    };
    use vessel_session::{
        Created, DEFAULT_DURABLE_CAPACITY, DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_RING_CAPACITY,
        Follow, LaunchOptions, RuntimeConfig, SessionError, SessionHandle, SessionRuntime,
    };
    use vessel_testkit::Fake;

    const SAFETY_NET: Duration = Duration::from_secs(30);

    pub(super) const SECRETS: [&str; 3] = [
        "stdio-secret-5d1",
        "header-secret-c07",
        "profile-secret-9e2",
    ];

    pub(super) async fn within<F: Future>(test: F) -> F::Output {
        vessel_testkit::within(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }

    /// A runtime whose search path holds the fakes, and a directory for
    /// profile homes.
    pub(super) struct Fixture {
        pub(super) runtime: SessionRuntime,
        dir: PathBuf,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "vessel-session-profiles-{}-{n}",
                std::process::id()
            ));
            // Not canonicalised: Windows would make it a `\\?\` path, which
            // `cmd.exe` cannot run a `.cmd` shim from inside a console.
            let bin = dir.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let tui = Fake::Tui.path().unwrap();
            let acp = Fake::Acp.path().unwrap();
            drop(install(&bin, HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
            drop(install(&bin, HarnessKind::Codex, Role::Terminal, &tui).unwrap());
            drop(install(&bin, HarnessKind::ClaudeCode, Role::Structured, &acp).unwrap());
            let runtime = SessionRuntime::new(
                RuntimeConfig {
                    harness_lookup: Lookup::new([bin]),
                    ring_capacity: DEFAULT_RING_CAPACITY,
                    durable_capacity: DEFAULT_DURABLE_CAPACITY,
                    agent_handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
                },
                CancellationToken::new(),
            );
            Self { runtime, dir }
        }

        /// Create a session as the node does: the spec names the profile
        /// the options carry.
        pub(super) async fn create(
            &self,
            spec: SessionSpec,
            options: LaunchOptions,
        ) -> Result<Created, SessionError> {
            let spec = match &options.profile {
                Some(profile) => spec.with_profile(profile.name().clone()),
                None => spec,
            };
            self.runtime.create_with(spec, options).await
        }

        /// Where profiles point their homes.
        pub(super) fn homes(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.dir));
        }
    }

    /// An inherited variable the fakes do not need, to remove.
    pub(super) fn inherited() -> &'static str {
        if cfg!(windows) { "USERPROFILE" } else { "HOME" }
    }

    /// `claude-alt` and `claude-work` with homes under `homes`, and a Codex
    /// profile; `claude-alt` removes [`inherited`] and holds a secret.
    pub(super) fn profiles(homes: &Path) -> Profiles {
        let text = format!(
            r#"
            [profiles.claude-alt]
            harness = "claude"
            env = {{ CLAUDE_CONFIG_DIR = '{alt}', ANTHROPIC_AUTH_TOKEN = "{secret}" }}
            unset = ["{unset}"]

            [profiles.claude-work]
            harness = "claude"
            env = {{ CLAUDE_CONFIG_DIR = '{work}' }}

            [profiles.codex-work]
            harness = "codex"
            "#,
            alt = homes.join(".claude-alt").display(),
            work = homes.join(".claude-work").display(),
            secret = SECRETS[2],
            unset = inherited(),
        );
        parse_profiles(&text, Some(homes)).unwrap()
    }

    pub(super) fn named(profiles: &Profiles, name: &str) -> Profile {
        profiles.get(&name.parse().unwrap()).unwrap()
    }

    pub(super) fn servers() -> ResolvedMcpServers {
        BTreeMap::from([
            (
                "docs".parse().unwrap(),
                McpTransport::Stdio {
                    command: PathBuf::from("/opt/docs-mcp"),
                    args: vec!["--stdio".to_owned()],
                    env: BTreeMap::from([(
                        "DOCS_TOKEN".parse().unwrap(),
                        ResolvedValue::new(SECRETS[0].to_owned()),
                    )]),
                },
            ),
            (
                "web".parse().unwrap(),
                McpTransport::Http {
                    url: "https://mcp.example.invalid/mcp".parse().unwrap(),
                    headers: BTreeMap::from([(
                        "Authorization".parse().unwrap(),
                        ResolvedValue::new(SECRETS[1].to_owned()),
                    )]),
                },
            ),
        ])
    }

    /// Wide, so a long path never wraps in the terminal.
    pub(super) fn terminal(harness: HarnessKind) -> SessionSpec {
        SessionSpec::new(
            harness,
            SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize {
                    cols: 1000,
                    rows: 24,
                },
            },
        )
        .with_cwd(std::env::temp_dir())
    }

    pub(super) fn structured(resume: Option<&str>) -> SessionSpec {
        SessionSpec::new(
            HarnessKind::ClaudeCode,
            SessionMode::Structured {
                model: None,
                resume: resume.map(HarnessSessionId::new),
            },
        )
        .with_cwd(std::env::temp_dir())
    }

    /// The first line of `text` holding `prefix`, from `prefix` to the end
    /// of the line or the first escape sequence (a Windows console redraws
    /// with them), with trailing spaces removed.
    pub(super) fn line(text: &str, prefix: &str) -> Option<String> {
        text.split(['\r', '\n']).find_map(|line| {
            let from = line.get(line.find(prefix)?..)?;
            let to = from.find('\u{1b}').unwrap_or(from.len());
            Some(from.get(..to)?.trim_end().to_owned())
        })
    }

    /// One session's output as text: terminal bytes, or agent message
    /// chunks one per line.
    pub(super) struct Reader {
        handle: SessionHandle,
        cached: VecDeque<Arc<EventRecord>>,
        follow: Follow,
        pub(super) text: String,
        turns: usize,
    }

    impl Reader {
        pub(super) fn new(handle: SessionHandle) -> Self {
            let follow = handle.follow(EventSeq::ZERO).unwrap();
            Self {
                cached: follow.cached.iter().cloned().collect(),
                follow,
                handle,
                text: String::new(),
                turns: 0,
            }
        }

        async fn next(&mut self) -> Option<Arc<EventRecord>> {
            let record = match self.cached.pop_front() {
                Some(record) => record,
                None => match self.follow.live.recv().await {
                    Ok(record) => record,
                    Err(RecvError::Closed) => return None,
                    Err(RecvError::Lagged(n)) => panic!("lagged by {n}"),
                },
            };
            match &record.event {
                SessionEvent::Terminal(output) => self
                    .text
                    .push_str(&String::from_utf8_lossy(output.data.bytes())),
                SessionEvent::Agent {
                    event:
                        AgentEvent::MessageChunk {
                            content: ContentBlock::Text { text },
                        },
                } => {
                    self.text.push_str(text);
                    self.text.push('\n');
                }
                SessionEvent::Agent {
                    event: AgentEvent::TurnEnded { .. },
                } => self.turns = self.turns.saturating_add(1),
                SessionEvent::Agent { .. }
                | SessionEvent::Status { .. }
                | SessionEvent::ProfileChanged { .. } => {}
            }
            Some(record)
        }

        /// Read until a line holds `prefix`; returns it from there.
        pub(super) async fn line(&mut self, prefix: &str) -> String {
            self.either(prefix, prefix).await
        }

        /// Read until a line holds one of two prefixes; returns it from
        /// there.
        pub(super) async fn either(&mut self, one: &str, other: &str) -> String {
            loop {
                if let Some(found) = line(&self.text, one).or_else(|| line(&self.text, other)) {
                    return found;
                }
                assert!(
                    self.next().await.is_some(),
                    "ended before {one:?} or {other:?}: {:?}",
                    self.text
                );
            }
        }

        /// Type a line into a terminal session.
        pub(super) async fn type_line(&self, text: &str) {
            let bytes = Bytes::from(format!("{text}\r"));
            self.handle
                .send(SessionInput::Terminal {
                    input: TerminalInput::Data {
                        data: TerminalBytes::new(bytes),
                    },
                })
                .await
                .unwrap();
        }

        /// Prompt a structured session and read its whole turn; returns
        /// the turn's one reply line.
        pub(super) async fn ask(&mut self, prompt: &str) -> String {
            let before = self.turns;
            let start = self.text.len();
            self.handle
                .send(SessionInput::Prompt {
                    content: vec![ContentBlock::Text {
                        text: prompt.to_owned(),
                    }],
                })
                .await
                .unwrap();
            while self.turns == before {
                assert!(
                    self.next().await.is_some(),
                    "ended mid-turn: {:?}",
                    self.text
                );
            }
            self.text
                .get(start..)
                .unwrap_or_default()
                .trim_end()
                .to_owned()
        }

        pub(super) async fn until_status(&mut self, wanted: &SessionStatus) {
            loop {
                let record = self.next().await.expect("the session ended first");
                if matches!(&record.event, SessionEvent::Status { status } if status == wanted) {
                    return;
                }
            }
        }

        pub(super) fn handle(&self) -> &SessionHandle {
            &self.handle
        }
    }
}

#[tokio::test]
async fn two_profiles_run_in_two_homes_and_unset_removes() {
    within(async {
        let fixture = Fixture::new();
        let profiles = profiles(fixture.homes());
        let mut seen = Vec::new();
        for name in ["claude-alt", "claude-work", "claude"] {
            let profile = named(&profiles, name);
            let created = fixture
                .create(
                    terminal(HarnessKind::ClaudeCode),
                    LaunchOptions {
                        profile: Some(profile.clone()),
                        ..LaunchOptions::default()
                    },
                )
                .await
                .unwrap();
            drop(created.durable);
            let mut reader = Reader::new(created.handle);
            let _ready = reader.line("ready").await;
            reader.type_line("env CLAUDE_CONFIG_DIR").await;
            let got = reader
                .either("env CLAUDE_CONFIG_DIR=", "env CLAUDE_CONFIG_DIR unset")
                .await;
            let expected = if profile.is_default() {
                // The default profile changes nothing: the node's own.
                std::env::var_os("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from)
            } else {
                home(&profile, fixture.homes(), |name| std::env::var_os(name))
            };
            match expected {
                Some(dir) => assert_eq!(got, format!("env CLAUDE_CONFIG_DIR={}", dir.display())),
                None => assert_eq!(got, "env CLAUDE_CONFIG_DIR unset"),
            }
            seen.push(got);

            reader.type_line(&format!("env {}", inherited())).await;
            let removed = format!("env {} unset", inherited());
            let report = reader
                .either(&removed, &format!("env {}=", inherited()))
                .await;
            assert_eq!(report == removed, name == "claude-alt", "{name}: {report}");
            reader.handle().kill();
            let _ended = reader.handle().ended().await;
        }
        assert_ne!(seen.first(), seen.get(1), "two profiles, two homes");
        fixture.runtime.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn another_harness_profile_is_refused() {
    within(async {
        let fixture = Fixture::new();
        let profiles = profiles(fixture.homes());
        let error = fixture
            .create(
                terminal(HarnessKind::ClaudeCode),
                LaunchOptions {
                    profile: Some(named(&profiles, "codex-work")),
                    ..LaunchOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                SessionError::Launch(HarnessError::ProfileMismatch { .. })
            ),
            "{error:?}"
        );
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
        // The default of another harness, too.
        let error = fixture
            .create(
                structured(None),
                LaunchOptions {
                    profile: Some(Profile::default_for(HarnessKind::Codex)),
                    ..LaunchOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
        // A spec naming a profile the node did not resolve is the node's
        // bug, not the client's.
        let error = fixture
            .runtime
            .create_with(
                terminal(HarnessKind::ClaudeCode).with_profile("claude-alt".parse().unwrap()),
                LaunchOptions::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                SessionError::Launch(HarnessError::ProfileNotRequested { .. })
            ),
            "{error:?}"
        );
        assert_eq!(error.code(), ErrorCode::Internal);
    })
    .await;
}

#[tokio::test]
async fn a_terminal_session_reads_its_servers_from_a_file_removed_with_it() {
    within(async {
        let fixture = Fixture::new();
        let created = fixture
            .create(
                terminal(HarnessKind::ClaudeCode),
                LaunchOptions {
                    profile: Some(named(&profiles(fixture.homes()), "claude-alt")),
                    mcp_servers: servers(),
                    mcp_dir: Some(fixture.homes().to_owned()),
                },
            )
            .await
            .unwrap();
        drop(created.durable);
        let mut reader = Reader::new(created.handle);
        let _ready = reader.line("ready").await;
        reader.type_line("args").await;
        let args = reader.line("args [").await;
        for secret in SECRETS {
            assert!(!args.contains(secret), "{secret} on argv: {args}");
        }
        let args: Vec<String> = serde_json::from_str(args.strip_prefix("args ").unwrap()).unwrap();
        let path = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--mcp-config="))
            .map(std::path::PathBuf::from)
            .unwrap();
        assert!(path.starts_with(fixture.homes()), "{}", path.display());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains(SECRETS[0]) && contents.contains(SECRETS[1]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        reader.type_line("exit 0").await;
        reader
            .until_status(&SessionStatus::Exited { code: Some(0) })
            .await;
        // The actor ends after its last status; shutdown waits for it.
        fixture.runtime.shutdown().await;
        assert!(!path.exists(), "{} outlived its session", path.display());
    })
    .await;
}

#[tokio::test]
async fn codex_terminals_get_values_in_the_environment_only() {
    within(async {
        let fixture = Fixture::new();
        let created = fixture
            .create(
                terminal(HarnessKind::Codex),
                LaunchOptions {
                    mcp_servers: servers(),
                    ..LaunchOptions::default()
                },
            )
            .await
            .unwrap();
        drop(created.durable);
        let mut reader = Reader::new(created.handle);
        let _ready = reader.line("ready").await;
        reader.type_line("args").await;
        let args = reader.line("args [").await;
        for secret in SECRETS {
            assert!(!args.contains(secret), "{secret} on argv: {args}");
        }
        assert!(args.contains("mcp_servers.docs="), "{args}");
        reader.type_line("env DOCS_TOKEN").await;
        assert_eq!(
            reader.line("env DOCS_TOKEN=").await,
            format!("env DOCS_TOKEN={}", SECRETS[0])
        );
        reader.handle().kill();
        let _ended = reader.handle().ended().await;
        fixture.runtime.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn structured_sessions_get_the_profile_and_servers_over_acp() {
    within(async {
        let fixture = Fixture::new();
        let profiles = profiles(fixture.homes());
        let alt = named(&profiles, "claude-alt");
        let options = LaunchOptions {
            profile: Some(alt.clone()),
            mcp_servers: servers(),
            mcp_dir: None,
        };
        let created = fixture
            .create(structured(None), options.clone())
            .await
            .unwrap();
        drop(created.durable);
        let mut reader = Reader::new(created.handle);
        reader.until_status(&SessionStatus::Running).await;

        let home = home(&alt, fixture.homes(), |name| std::env::var_os(name)).unwrap();
        assert_eq!(
            reader.ask("env CLAUDE_CONFIG_DIR").await,
            format!("env CLAUDE_CONFIG_DIR={}", home.display())
        );
        assert_eq!(
            reader.ask(&format!("env {}", inherited())).await,
            format!("env {} unset", inherited())
        );
        let mcp = reader.ask("mcp").await;
        let json: serde_json::Value =
            serde_json::from_str(mcp.strip_prefix("mcp ").unwrap()).unwrap();
        assert_eq!(json[0]["name"], "docs");
        assert_eq!(json[0]["env"][0]["value"], SECRETS[0]);
        assert_eq!(json[1]["headers"][0]["value"], SECRETS[1]);
        let args = reader.ask("args").await;
        for secret in SECRETS {
            assert!(!args.contains(secret), "{secret} on argv: {args}");
        }
        assert_eq!(reader.ask("loaded").await, "loaded none");
        reader.handle().kill();
        let _ended = reader.handle().ended().await;

        // Resumed: the same servers again, and the id that was loaded.
        let created = fixture
            .create(structured(Some("fake-session-3")), options)
            .await
            .unwrap();
        drop(created.durable);
        let mut reader = Reader::new(created.handle);
        reader.until_status(&SessionStatus::Running).await;
        assert_eq!(reader.ask("loaded").await, "loaded fake-session-3");
        assert_eq!(
            reader.handle().info().harness_session,
            Some(HarnessSessionId::new("fake-session-3"))
        );
        let mcp = reader.ask("mcp").await;
        assert!(mcp.contains(SECRETS[0]), "{mcp}");
        fixture.runtime.shutdown().await;
    })
    .await;
}
