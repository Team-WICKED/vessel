//! Terminal mode: `fake-tui` as `claude`.

use vessel_client::{CallError, NodeClient};
use vessel_proto::{
    ContentBlock, ErrorCode, EventSeq, HarnessKind, SessionInput, SessionStatus, TerminalInput,
    TerminalSize,
};

use crate::support::{
    Fixture, collect_until, keys, output_of, read_all, seqs, status_of, terminal_spec, within,
};

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
        let refused = NodeClient::connect(
            &crate::support::url(&node),
            &wrong,
            crate::support::client(),
        )
        .await;
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

/// Regression: with retention pruning the store past the ring, a late
/// follower from zero used to make the node ask the ring for the same gap
/// forever. It now gets the pruned range reported once, then the ring, then
/// the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gap_pruned_from_the_store_is_reported_once_then_the_ring_follows() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture
            .start_with(|config| {
                config.ring_capacity = std::num::NonZeroUsize::new(4).unwrap();
                config.store.retention.max_session_bytes = vessel_node::ByteCount::new(1);
            })
            .await;
        let client = fixture.connect(&node).await;
        let id = client
            .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
            .await
            .unwrap()
            .id;
        let mut first = client.follow(id, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        collect_until(&mut first, &mut seen, "> ").await;
        for line in ["one\n", "two\n", "three\n", "exit\n"] {
            client.send(id, keys(line)).await.unwrap();
        }
        // The first follower lags too; it only has to reach the end.
        let mut last = EventSeq::ZERO;
        while let Some(record) = first.next().await {
            match record {
                Ok(record) => last = record.seq,
                Err(error) => assert!(!error.is_fatal(), "{error}"),
            }
        }
        assert!(last.get() >= 8, "too few records to leave a gap: {last}");

        let late = fixture.connect(&node).await;
        let mut replay = late.follow(id, EventSeq::ZERO).await.unwrap();
        let oldest_cached = EventSeq::new(last.get() - 3);
        match replay.next().await {
            Some(Err(vessel_client::StreamError::Missing {
                expected,
                resumed_at,
            })) => {
                assert_eq!(expected, EventSeq::ZERO);
                assert_eq!(resumed_at, oldest_cached);
            }
            other => panic!("expected the pruned range first, got {other:?}"),
        }
        let mut rest = Vec::new();
        while let Some(record) = replay.next().await {
            rest.push(record.unwrap());
        }
        let expected: Vec<u64> = (oldest_cached.get()..=last.get()).collect();
        assert_eq!(seqs(&rest), expected);
        assert!(matches!(
            rest.last().and_then(status_of),
            Some(SessionStatus::Exited { .. })
        ));
        node.stop().await.unwrap();
    })
    .await;
}
