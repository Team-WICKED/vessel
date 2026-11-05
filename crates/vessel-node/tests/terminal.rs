//! The node end to end in terminal mode: an in-process `vesseld` on a
//! random loopback port with a temporary data directory, `fake-tui`
//! installed as `claude`, driven only through `vessel-client`.
//!
//! Tests wait on records and replies, never on time; the timeout only turns
//! a hang into a failure.

use vessel_client::{CallError, NodeClient};
use vessel_proto::{
    ContentBlock, ErrorCode, EventSeq, HarnessKind, SessionInput, SessionStatus, TerminalInput,
    TerminalSize,
};

use support::{
    Fixture, collect_until, keys, output_of, read_all, seqs, status_of, terminal_spec, within,
};

#[cfg(test)]
mod support {
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
        ClientInfo, EventRecord, EventSeq, HarnessKind, SessionEvent, SessionId, SessionInput,
        SessionMode, SessionSpec, SessionStatus, TerminalBytes, TerminalInput, TerminalSize,
    };
    use vessel_testkit::Fake;

    const SAFETY_NET: Duration = Duration::from_secs(60);

    pub(super) async fn within<F: Future>(test: F) -> F::Output {
        tokio::time::timeout(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }

    /// A data directory and a harness directory holding `fake-tui` as
    /// `claude`, shared by every node started from this fixture.
    pub(super) struct Fixture {
        data: TempDir,
        bin: TempDir,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            let data = tempfile::tempdir().unwrap();
            let bin = tempfile::tempdir().unwrap();
            let tui = Fake::Tui.path().unwrap();
            drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
            Self { data, bin }
        }

        pub(super) fn data_dir(&self) -> DataDir {
            DataDir::new(self.data.path())
        }

        pub(super) async fn start(&self) -> Node {
            self.start_with(|_| {}).await
        }

        pub(super) async fn start_with(&self, adjust: impl FnOnce(&mut Config)) -> Node {
            let mut config = Config::new(self.data_dir());
            config.bind = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
            config.harness_lookup = Lookup::new([self.bin.path().to_path_buf()]);
            adjust(&mut config);
            Node::start(config).await.unwrap()
        }

        pub(super) fn token(&self) -> BearerToken {
            self.data_dir()
                .read_token()
                .unwrap()
                .expose()
                .parse()
                .unwrap()
        }

        pub(super) async fn connect(&self, node: &Node) -> NodeClient {
            NodeClient::connect(&url(node), &self.token(), client())
                .await
                .unwrap()
        }

        pub(super) fn cwd(&self) -> std::path::PathBuf {
            self.data.path().to_path_buf()
        }
    }

    pub(super) fn url(node: &Node) -> NodeUrl {
        node.address().to_string().parse().unwrap()
    }

    pub(super) fn client() -> ClientInfo {
        ClientInfo {
            name: "vessel-node-e2e".to_owned(),
            version: "0".to_owned(),
        }
    }

    pub(super) fn terminal_spec(fixture: &Fixture, harness: HarnessKind) -> SessionSpec {
        SessionSpec {
            harness,
            cwd: Some(fixture.cwd()),
            mode: SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            },
        }
    }

    pub(super) fn keys(text: &'static str) -> SessionInput {
        SessionInput::Terminal {
            input: TerminalInput::Data {
                data: TerminalBytes::new(Bytes::from_static(text.as_bytes())),
            },
        }
    }

    /// The terminal output in `records`, as text.
    pub(super) fn output_of(records: &[EventRecord]) -> String {
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
    pub(super) async fn collect_until(
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

    pub(super) fn seqs(records: &[EventRecord]) -> Vec<u64> {
        records.iter().map(|record| record.seq.get()).collect()
    }

    pub(super) fn status_of(record: &EventRecord) -> Option<&SessionStatus> {
        match &record.event {
            SessionEvent::Status { status } => Some(status),
            SessionEvent::Terminal(_) | SessionEvent::Agent { .. } => None,
        }
    }

    /// Read a whole log with `session.read`, page by page.
    pub(super) async fn read_all(client: &NodeClient, session: SessionId) -> Vec<EventRecord> {
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_session_survives_clients_and_a_node_restart() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;
        assert_eq!(client.node().id, node.info().id);
        let harnesses = client.harnesses().await.unwrap();
        assert!(
            harnesses
                .iter()
                .any(|harness| harness.kind == HarnessKind::ClaudeCode)
        );

        let info = client
            .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let id = info.id;
        let mut live = client.follow(id, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        collect_until(&mut live, &mut seen, "> ").await;
        client.send(id, keys("hello vessel\n")).await.unwrap();
        collect_until(&mut live, &mut seen, "echo: hello vessel").await;
        if cfg!(unix) {
            let resize = SessionInput::Terminal {
                input: TerminalInput::Resize {
                    size: TerminalSize {
                        cols: 120,
                        rows: 40,
                    },
                },
            };
            client.send(id, resize).await.unwrap();
            collect_until(&mut live, &mut seen, "size 120x40").await;
        }
        // Gap-free from zero, and it starts like every session.
        let expected: Vec<u64> = (0..u64::try_from(seen.len()).unwrap()).collect();
        assert_eq!(seqs(&seen), expected);
        assert_eq!(
            status_of(&seen[0]),
            Some(&SessionStatus::Starting),
            "first record"
        );

        // A second client that attaches late gets the same history.
        let late = fixture.connect(&node).await;
        let mut replay = late.follow(id, EventSeq::ZERO).await.unwrap();
        let mut replayed = Vec::new();
        collect_until(&mut replay, &mut replayed, "echo: hello vessel").await;
        assert_eq!(replayed[..], seen[..replayed.len()]);

        // `session.read` serves everything the session produced so far.
        let read = read_all(&client, id).await;
        assert!(read.len() >= seen.len());
        assert_eq!(read[..seen.len()], seen[..]);

        // Structured input does not apply to a terminal session.
        let prompt = SessionInput::Prompt {
            content: vec![ContentBlock::Text {
                text: "hi".to_owned(),
            }],
        };
        assert_eq!(
            client.send(id, prompt).await.unwrap_err().code(),
            Some(ErrorCode::Unsupported)
        );

        // Stop the node: the session ends as interrupted, and the live
        // subscriber receives that before its stream ends.
        node.stop().await.unwrap();
        let mut tail = Vec::new();
        while let Some(record) = live.next().await {
            tail.push(record.unwrap());
        }
        assert_eq!(
            tail.last().and_then(status_of),
            Some(&SessionStatus::Interrupted)
        );

        // Restart on the same data directory: the session is listed as
        // interrupted and its whole log reads back.
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;
        let listed = client.sessions().await.unwrap();
        let restored = listed.iter().find(|session| session.id == id).unwrap();
        assert_eq!(restored.status, SessionStatus::Interrupted);
        assert!(restored.ended_at.is_some());

        let log = read_all(&client, id).await;
        let expected: Vec<u64> = (0..u64::try_from(log.len()).unwrap()).collect();
        assert_eq!(seqs(&log), expected);
        assert_eq!(log[..seen.len()], seen[..]);
        assert!(output_of(&log).contains("echo: hello vessel"));
        assert_eq!(
            log.last().and_then(status_of),
            Some(&SessionStatus::Interrupted)
        );
        assert_eq!(restored.next_seq.get(), u64::try_from(log.len()).unwrap());

        // Following a stored session replays it and ends.
        let mut stored = client.follow(id, EventSeq::ZERO).await.unwrap();
        let mut followed = Vec::new();
        while let Some(record) = stored.next().await {
            followed.push(record.unwrap());
        }
        assert_eq!(followed, log);

        // It can be read, not driven.
        assert_eq!(
            client.send(id, keys("again\n")).await.unwrap_err().code(),
            Some(ErrorCode::Conflict)
        );
        node.stop().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_sessions_end_and_errors_carry_their_codes() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;

        let info = client
            .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let mut stream = client.follow(info.id, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        collect_until(&mut stream, &mut seen, "> ").await;
        client.kill(info.id).await.unwrap();
        while let Some(record) = stream.next().await {
            seen.push(record.unwrap());
        }
        let last = seen.last().and_then(status_of).unwrap();
        assert!(
            matches!(last, SessionStatus::Exited { .. }),
            "killed session ended with {last:?}"
        );
        let ended = client.session(info.id).await.unwrap();
        assert_eq!(&ended.status, last);
        // Killing again is harmless; input is refused.
        client.kill(info.id).await.unwrap();
        assert_eq!(
            client.send(info.id, keys("x\n")).await.unwrap_err().code(),
            Some(ErrorCode::Conflict)
        );
        // Subscribing past the end is an invalid request.
        let past = ended.next_seq.next().unwrap();
        assert_eq!(
            client.follow(info.id, past).await.unwrap_err().code(),
            Some(ErrorCode::InvalidRequest)
        );

        let missing = client
            .create_session(terminal_spec(&fixture, HarnessKind::Codex))
            .await
            .unwrap_err();
        assert_eq!(missing.code(), Some(ErrorCode::HarnessNotInstalled));
        let unknown = vessel_proto::SessionId::random();
        for error in [
            client.session(unknown).await.unwrap_err(),
            client.read(unknown, EventSeq::ZERO, 10).await.unwrap_err(),
            client.follow(unknown, EventSeq::ZERO).await.unwrap_err(),
        ] {
            assert_eq!(error.code(), Some(ErrorCode::NotFound), "{error}");
        }
        node.stop().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_need_the_token() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start().await;
        let wrong: vessel_client::BearerToken = "not-the-token".parse().unwrap();
        let refused = NodeClient::connect(&support::url(&node), &wrong, support::client()).await;
        assert!(
            matches!(refused, Err(vessel_client::ConnectError::Unauthorized)),
            "{refused:?}"
        );
        let client = fixture.connect(&node).await;
        assert_eq!(client.sessions().await.unwrap(), Vec::new());
        drop(client);
        node.stop().await.unwrap();
    })
    .await;
}

/// Calls made while the node stops get a clean answer or a disconnect, never
/// a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnected_client_reports_it() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start().await;
        let client = fixture.connect(&node).await;
        node.stop().await.unwrap();
        client.closed().await;
        assert_eq!(client.sessions().await, Err(CallError::Disconnected));
    })
    .await;
}

/// With a ring of two records, a late follower's history comes from the
/// store (after the node waited for it to be committed), and live delivery
/// lags constantly; the stream must still be complete and in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tiny_rings_force_store_gap_fills_and_lag_without_losing_records() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture
            .start_with(|config| {
                config.ring_capacity = std::num::NonZeroUsize::new(2).unwrap();
            })
            .await;
        let client = fixture.connect(&node).await;
        let info = client
            .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let id = info.id;
        let mut first = client.follow(id, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        collect_until(&mut first, &mut seen, "> ").await;
        for line in ["one\n", "two\n", "three\n", "four\n", "five\n"] {
            client.send(id, keys(line)).await.unwrap();
        }
        collect_until(&mut first, &mut seen, "echo: five").await;

        let late = fixture.connect(&node).await;
        let mut replay = late.follow(id, EventSeq::ZERO).await.unwrap();
        let mut replayed = Vec::new();
        collect_until(&mut replay, &mut replayed, "echo: five").await;
        for stream in [&seen, &replayed] {
            let expected: Vec<u64> = (0..u64::try_from(stream.len()).unwrap()).collect();
            assert_eq!(seqs(stream), expected);
        }
        assert_eq!(replayed[..], seen[..replayed.len()]);
        node.stop().await.unwrap();
    })
    .await;
}
