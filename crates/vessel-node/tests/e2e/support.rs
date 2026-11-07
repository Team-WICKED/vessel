//! Shared fixture: a temporary data directory and a harness directory
//! where `fake-tui` is `claude` and `fake-acp` is Claude's ACP adapter
//! (`claude-agent-acp`), so both session modes run through the real lookup
//! and launch code.

use std::fmt;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Once;
use std::time::Duration;

use bytes::Bytes;
use tempfile::TempDir;
use vessel_client::{BearerToken, NodeClient, NodeUrl, SessionStream};
use vessel_harness::fakes::{Role, install};
use vessel_harness::{HarnessEnv, Lookup};
use vessel_node::{Config, DataDir, Node};
use vessel_proto::{
    AgentEvent, ClientInfo, ContentBlock, EventRecord, EventSeq, HarnessKind, HarnessSessionId,
    ModelId, SessionEvent, SessionId, SessionInput, SessionMode, SessionSpec, SessionStatus,
    TerminalBytes, TerminalInput, TerminalSize,
};
use vessel_testkit::Fake;

const SAFETY_NET: Duration = Duration::from_secs(60);

/// How long one awaited step may take ([`step`], [`collect_until`]): well
/// under [`SAFETY_NET`], so a hang fails naming the step it is in rather
/// than as "test timed out".
pub(crate) const STEP: Duration = Duration::from_secs(20);

/// What the logs show by default ([`logs`]); `RUST_LOG` replaces it.
const LOG_FILTER: &str = "info,vessel_node=debug,vessel_relay=debug,vessel_client=debug,\
    vessel_rpc=debug,vessel_secure=debug,vessel_session=debug,vessel_pty=debug,\
    vessel_testkit=debug";

/// The safety net starts after the fakes are built: the first build of a
/// run can take a minute on a cold machine (see `vessel_testkit::within`).
/// The test is boxed at once, so the future handed around stays small.
pub(crate) fn within<F: Future>(test: F) -> impl Future<Output = F::Output> {
    logs();
    let test = Box::pin(test);
    async move {
        vessel_testkit::within(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }
}

/// Send the node's, relay's and client's logs (debug, see [`LOG_FILTER`])
/// to the output libtest keeps for each test and prints only for one that
/// fails, once per test binary.
///
/// [Decision Log]
/// - Purpose: a hang seen once on one CI runner (a Windows run of the
///   tampering-relay test) left nothing but "test timed out". The logs of
///   that run say which connection ended, when, and why.
/// - How it reaches the right test: libtest captures output per thread,
///   and a thread inherits the capture of the thread that spawned it.
///   `#[tokio::test]` builds its runtime on the test's thread, so its
///   workers, their blocking threads and the node's PTY threads all write
///   into that test's capture; [`step`] logs each step there too, so the
///   logs read in the test's own order.
/// - Alternatives: logs only when a test asks for them (the next hang may
///   be in a test that did not); a per-test subscriber (`set_default` is
///   per thread and does not follow tasks onto other workers).
/// - Cost: formatting debug events for every test, kept in memory until
///   the test ends. Nothing is printed for a test that passes.
fn logs() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_unset| tracing_subscriber::EnvFilter::new(LOG_FILTER));
        let installed = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_test_writer()
            .try_init();
        // Another subscriber was installed first: its logs are kept instead.
        drop(installed);
    });
}

/// Await `future`, failing with `what` if it takes longer than [`STEP`].
/// A test that hangs then says which step, of which round, it hung in,
/// and the step is in the test's logs ([`logs`]) in order with the node's.
/// Logged when called, so call it where it is awaited. `future` is boxed,
/// so a step's future stays small whatever it wraps.
pub(crate) fn step<F: Future>(
    what: impl fmt::Display,
    future: F,
) -> impl Future<Output = F::Output> {
    let what = what.to_string();
    tracing::info!("step: {what}");
    let future = Box::pin(future);
    async move {
        tokio::time::timeout(STEP, future)
            .await
            .unwrap_or_else(|_elapsed| panic!("stuck for {} s: {what}", STEP.as_secs()))
    }
}

/// The last `chars` characters of `text`, for a failure message.
pub(crate) fn tail(text: &str, chars: usize) -> String {
    let mut last: Vec<char> = text.chars().rev().take(chars).collect();
    last.reverse();
    last.into_iter().collect()
}

/// A data directory and a harness directory, shared by every node started
/// from this fixture (so a restarted node finds the same state).
pub(crate) struct Fixture {
    data: TempDir,
    bin: TempDir,
    /// The user home its nodes run harnesses under: never the machine's.
    home: TempDir,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self::with_adapter(|bin, acp| {
            drop(install(bin, HarnessKind::ClaudeCode, Role::Structured, acp).unwrap());
        })
    }

    /// Also Codex: fake-tui as `codex`, fake-acp as `codex-acp`.
    pub(crate) fn with_codex() -> Self {
        Self::with_adapter(|bin, acp| {
            drop(install(bin, HarnessKind::ClaudeCode, Role::Structured, acp).unwrap());
            drop(install(bin, HarnessKind::Codex, Role::Structured, acp).unwrap());
            let tui = Fake::Tui.path().unwrap();
            drop(install(bin, HarnessKind::Codex, Role::Terminal, &tui).unwrap());
        })
    }

    /// Claude's ACP adapter never answers `initialize`: fake-acp with
    /// `FAKE_ACP_HANG`, behind a wrapper that sets it. (Written instead of
    /// the usual install, which on Unix is a symlink to the shared fake.)
    pub(crate) fn with_stuck_adapter() -> Self {
        Self::with_adapter(|bin, acp| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let wrapper = bin.join("claude-agent-acp");
                let script = format!(
                    "#!/bin/sh\nFAKE_ACP_HANG=initialize exec '{}' \"$@\"\n",
                    acp.display()
                );
                std::fs::write(&wrapper, script).unwrap();
                std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            #[cfg(windows)]
            std::fs::write(
                bin.join("claude-agent-acp.cmd"),
                format!(
                    "@echo off\r\nset FAKE_ACP_HANG=initialize\r\n\"{}\" %*\r\nexit /b %ERRORLEVEL%\r\n",
                    acp.display()
                ),
            )
            .unwrap();
        })
    }

    fn with_adapter(adapter: impl FnOnce(&std::path::Path, &std::path::Path)) -> Self {
        let data = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let tui = Fake::Tui.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
        adapter(bin.path(), &Fake::Acp.path().unwrap());
        Self {
            data,
            bin,
            home: tempfile::tempdir().unwrap(),
        }
    }

    /// The account its nodes' harnesses run as: a temporary home and no
    /// home variables, so a default profile's home is under it.
    pub(crate) fn harness_env(&self) -> HarnessEnv {
        HarnessEnv::new(self.home.path().to_owned())
    }

    /// That temporary user home.
    pub(crate) fn user_home(&self) -> &std::path::Path {
        self.home.path()
    }

    pub(crate) fn data_dir(&self) -> DataDir {
        DataDir::new(self.data.path())
    }

    pub(crate) async fn start(&self) -> Node {
        self.start_with(|_| {}).await
    }

    pub(crate) async fn start_with(&self, adjust: impl FnOnce(&mut Config)) -> Node {
        let mut config = Config::new(self.data_dir(), self.harness_env());
        config.bind = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
        config.harness_lookup = Lookup::new([self.bin.path().to_path_buf()]);
        adjust(&mut config);
        Node::start(config).await.unwrap()
    }

    pub(crate) fn token(&self) -> BearerToken {
        self.data_dir()
            .read_token()
            .unwrap()
            .expose()
            .parse()
            .unwrap()
    }

    pub(crate) async fn connect(&self, node: &Node) -> NodeClient {
        NodeClient::connect(&url(node), &self.token(), client())
            .await
            .unwrap()
    }

    pub(crate) fn cwd(&self) -> std::path::PathBuf {
        self.data.path().to_path_buf()
    }
}

pub(crate) fn url(node: &Node) -> NodeUrl {
    node.address().unwrap().to_string().parse().unwrap()
}

pub(crate) fn client() -> ClientInfo {
    ClientInfo {
        name: "vessel-node-e2e".to_owned(),
        version: "0".to_owned(),
    }
}

pub(crate) fn terminal_spec(fixture: &Fixture, harness: HarnessKind) -> SessionSpec {
    SessionSpec::new(
        harness,
        SessionMode::Terminal {
            args: Vec::new(),
            size: TerminalSize::default(),
        },
    )
    .with_cwd(fixture.cwd())
}

/// Keys typed into a terminal session. End lines with `\r`, as a terminal
/// does for Enter: Unix PTYs turn it into `\n` (`ICRNL`), while `ConPTY` on
/// Windows completes a console line only on `\r` (`\n` is Ctrl-J there).
pub(crate) fn keys(text: &'static str) -> SessionInput {
    SessionInput::Terminal {
        input: TerminalInput::Data {
            data: TerminalBytes::new(Bytes::from_static(text.as_bytes())),
        },
    }
}

/// The terminal output in `records`, as text.
pub(crate) fn output_of(records: &[EventRecord]) -> String {
    let bytes: Vec<u8> = records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::Terminal(output) => Some(output.data.bytes().to_vec()),
            SessionEvent::Status { .. }
            | SessionEvent::Agent { .. }
            | SessionEvent::ProfileChanged { .. }
            | SessionEvent::ModelCall(_) => None,
        })
        .flatten()
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Read records into `seen` until the output contains `needle`. Fails
/// after [`STEP`] showing the end of the output, escaped: a terminal's
/// output can hold the text with control sequences inside it.
pub(crate) async fn collect_until(
    stream: &mut SessionStream,
    seen: &mut Vec<EventRecord>,
    needle: &str,
) {
    tracing::info!("step: reading the output until {needle:?}");
    let reading = async {
        while !output_of(seen).contains(needle) {
            let record = stream
                .next()
                .await
                .unwrap_or_else(|| panic!("stream ended before {needle:?}"))
                .unwrap();
            seen.push(record);
        }
    };
    let read = tokio::time::timeout(STEP, reading).await;
    assert!(
        read.is_ok(),
        "no {needle:?} in the output after {} s; it ends with {:?}",
        STEP.as_secs(),
        tail(&output_of(seen), 600)
    );
}

pub(crate) fn seqs(records: &[EventRecord]) -> Vec<u64> {
    records.iter().map(|record| record.seq.get()).collect()
}

pub(crate) fn status_of(record: &EventRecord) -> Option<&SessionStatus> {
    match &record.event {
        SessionEvent::Status { status } => Some(status),
        SessionEvent::Terminal(_)
        | SessionEvent::Agent { .. }
        | SessionEvent::ProfileChanged { .. }
        | SessionEvent::ModelCall(_) => None,
    }
}

/// Read a whole log with `session.read`, page by page.
pub(crate) async fn read_all(client: &NodeClient, session: SessionId) -> Vec<EventRecord> {
    let mut records = Vec::new();
    let mut from = EventSeq::ZERO;
    loop {
        let page = client.read(session, from, 7).await.unwrap();
        records.extend(page.records);
        from = page.next;
        if page.at_end {
            return records;
        }
    }
}

/// A new structured session asks for `fake-large`; a resumed one keeps
/// the model it had (the agent offers no model choice on `session/load`).
pub(crate) fn structured_spec(fixture: &Fixture, resume: Option<HarnessSessionId>) -> SessionSpec {
    let model = resume.is_none().then(|| ModelId::new("fake-large"));
    SessionSpec::new(
        HarnessKind::ClaudeCode,
        SessionMode::Structured { model, resume },
    )
    .with_cwd(fixture.cwd())
}

pub(crate) fn prompt(text: &str) -> SessionInput {
    SessionInput::Prompt {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    }
}

/// The agent event of `record`, if it has one.
pub(crate) const fn agent_of(record: &EventRecord) -> Option<&AgentEvent> {
    match &record.event {
        SessionEvent::Agent { event } => Some(event),
        SessionEvent::Status { .. }
        | SessionEvent::Terminal(_)
        | SessionEvent::ProfileChanged { .. }
        | SessionEvent::ModelCall(_) => None,
    }
}

/// Read records into `seen` until one matches `wanted` (it is then
/// `seen.last()`).
pub(crate) async fn read_until(
    stream: &mut SessionStream,
    seen: &mut Vec<EventRecord>,
    wanted: impl Fn(&EventRecord) -> bool,
) {
    loop {
        let Some(record) = stream.next().await else {
            let events: Vec<_> = seen.iter().map(|record| &record.event).collect();
            panic!("stream ended before the expected record; saw {events:#?}");
        };
        let record = record.unwrap();
        let found = wanted(&record);
        seen.push(record);
        if found {
            return;
        }
    }
}

/// Prompt a structured session and return its reply text.
pub(crate) async fn ask(
    client: &NodeClient,
    stream: &mut SessionStream,
    seen: &mut Vec<EventRecord>,
    session: SessionId,
    text: &str,
) -> String {
    let start = seen.len();
    client.send(session, prompt(text)).await.unwrap();
    read_until(stream, seen, |record| {
        matches!(agent_of(record), Some(AgentEvent::TurnEnded { .. }))
    })
    .await;
    seen[start..]
        .iter()
        .filter_map(|record| match agent_of(record) {
            Some(AgentEvent::MessageChunk {
                content: ContentBlock::Text { text },
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
