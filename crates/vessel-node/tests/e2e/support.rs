//! Shared fixture: a temporary data directory and a harness directory
//! where `fake-tui` is `claude` and `fake-acp` is Claude's ACP adapter
//! (`claude-agent-acp`), so both session modes run through the real lookup
//! and launch code.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use tempfile::TempDir;
use vessel_client::{BearerToken, NodeClient, NodeUrl, SessionStream};
use vessel_harness::Lookup;
use vessel_harness::fakes::{Role, install};
use vessel_node::{Config, DataDir, Node};
use vessel_proto::{
    AgentEvent, ClientInfo, ContentBlock, EventRecord, EventSeq, HarnessKind, HarnessSessionId,
    ModelId, SessionEvent, SessionId, SessionInput, SessionMode, SessionSpec, SessionStatus,
    TerminalBytes, TerminalInput, TerminalSize,
};
use vessel_testkit::Fake;

const SAFETY_NET: Duration = Duration::from_secs(60);

/// The safety net starts after the fakes are built: the first build of a
/// run can take a minute on a cold machine (see `vessel_testkit::within`).
/// The test is boxed at once, so the future handed around stays small.
pub(crate) fn within<F: Future>(test: F) -> impl Future<Output = F::Output> {
    let test = Box::pin(test);
    async move {
        vessel_testkit::within(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }
}

/// A data directory and a harness directory, shared by every node started
/// from this fixture (so a restarted node finds the same state).
pub(crate) struct Fixture {
    data: TempDir,
    bin: TempDir,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let tui = Fake::Tui.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
        let acp = Fake::Acp.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Structured, &acp).unwrap());
        Self { data, bin }
    }

    pub(crate) fn data_dir(&self) -> DataDir {
        DataDir::new(self.data.path())
    }

    pub(crate) async fn start(&self) -> Node {
        self.start_with(|_| {}).await
    }

    pub(crate) async fn start_with(&self, adjust: impl FnOnce(&mut Config)) -> Node {
        let mut config = Config::new(self.data_dir());
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
    SessionSpec {
        harness,
        cwd: Some(fixture.cwd()),
        mode: SessionMode::Terminal {
            args: Vec::new(),
            size: TerminalSize::default(),
        },
    }
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
            SessionEvent::Status { .. } | SessionEvent::Agent { .. } => None,
        })
        .flatten()
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Read records into `seen` until the output contains `needle`.
pub(crate) async fn collect_until(
    stream: &mut SessionStream,
    seen: &mut Vec<EventRecord>,
    needle: &str,
) {
    while !output_of(seen).contains(needle) {
        let record = stream
            .next()
            .await
            .unwrap_or_else(|| panic!("stream ended before {needle:?}"))
            .unwrap();
        seen.push(record);
    }
}

pub(crate) fn seqs(records: &[EventRecord]) -> Vec<u64> {
    records.iter().map(|record| record.seq.get()).collect()
}

pub(crate) fn status_of(record: &EventRecord) -> Option<&SessionStatus> {
    match &record.event {
        SessionEvent::Status { status } => Some(status),
        SessionEvent::Terminal(_) | SessionEvent::Agent { .. } => None,
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
    SessionSpec {
        harness: HarnessKind::ClaudeCode,
        cwd: Some(fixture.cwd()),
        mode: SessionMode::Structured { model, resume },
    }
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
        SessionEvent::Status { .. } | SessionEvent::Terminal(_) => None,
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
