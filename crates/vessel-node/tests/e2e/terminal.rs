//! Terminal mode: `fake-tui` as `claude`.

use vessel_client::{CallError, ClientConfig, NodeClient, SessionStream};
use vessel_proto::{
    ContentBlock, ErrorCode, EventRecord, EventSeq, HarnessKind, SessionInput, SessionStatus,
    TerminalInput, TerminalSize,
};

use crate::support::{
    Fixture, client as client_info, collect_until, keys, output_of, read_all, seqs, status_of,
    terminal_spec, url, within,
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
        client.send(id, keys("hello vessel\r")).await.unwrap();
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
            client.send(id, keys("again\r")).await.unwrap_err().code(),
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
            client.send(info.id, keys("x\r")).await.unwrap_err().code(),
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
        let refused = NodeClient::connect(&url(&node), &wrong, crate::support::client()).await;
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
        for line in ["one\r", "two\r", "three\r", "four\r", "five\r"] {
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

/// A follower that falls behind catches up with `session.read`. When
/// retention has pruned records from a live session's log that its ring
/// still holds, the read serves them from the ring: the follower misses
/// only what the node no longer has at all. (Reading only the store skipped
/// them, and a follower waiting for that output waited forever: the flake
/// in `a_gap_pruned_from_the_store_is_reported_once_then_the_ring_follows`.)
///
/// Returns the follower's records up to "echo: three", and the gaps it
/// reported. The output is produced before the follower reads, so the
/// catch-up happens with the session already idle.
async fn catch_up_after_pruning(ring: usize) -> (Vec<EventRecord>, Vec<Gap>) {
    let fixture = Fixture::new();
    let node = fixture
        .start_with(|config| {
            config.ring_capacity = std::num::NonZeroUsize::new(ring).unwrap();
            config.store.retention.max_session_bytes = vessel_node::ByteCount::new(1);
        })
        .await;
    let client = fixture.connect(&node).await;
    // One record of client-side buffer: unread, this follower lags at its
    // second record.
    let tight = ClientConfig {
        subscription_capacity: std::num::NonZeroUsize::MIN,
        ..ClientConfig::default()
    };
    let slow = NodeClient::connect_with(&url(&node), &fixture.token(), client_info(), &tight)
        .await
        .unwrap();
    let id = client
        .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
        .await
        .unwrap()
        .id;
    let mut lagging = slow.follow(id, EventSeq::ZERO).await.unwrap();
    let mut watcher = client.follow(id, EventSeq::ZERO).await.unwrap();
    let mut progress = Vec::new();
    watch_until(&mut watcher, &mut progress, "> ").await;
    for line in ["one\r", "two\r", "three\r"] {
        client.send(id, keys(line)).await.unwrap();
    }
    watch_until(&mut watcher, &mut progress, "echo: three").await;

    let mut seen = Vec::new();
    let mut gaps = Vec::new();
    let caught_up = async {
        while !output_of(&seen).contains("echo: three") {
            match lagging.next().await.unwrap() {
                Ok(record) => seen.push(record),
                Err(vessel_client::StreamError::Missing {
                    expected,
                    resumed_at,
                }) => gaps.push(Gap {
                    expected,
                    resumed_at,
                    at: seen.len(),
                }),
                Err(error) => panic!("{error}"),
            }
        }
    };
    // The session is idle by now: output the follower skipped never comes.
    tokio::time::timeout(std::time::Duration::from_secs(20), caught_up)
        .await
        .expect("the follower skipped output the node still had");

    client.kill(id).await.unwrap();
    drop((lagging, watcher, slow, client));
    node.stop().await.unwrap();
    (seen, gaps)
}

/// A gap a follower reported: records `expected..resumed_at` are gone, and
/// it came after the follower's first `at` records.
#[derive(Debug)]
struct Gap {
    expected: EventSeq,
    resumed_at: EventSeq,
    at: usize,
}

/// Read `watcher` until its output holds `needle`, accepting reported
/// gaps.
///
/// The watcher only tells the test when output is done, and it lives under
/// the same retention as the follower under test: with a ring of four and
/// the store pruned to one record, it is told records are missing whenever
/// it falls four behind. That happens under load in two ways: the start-up
/// output (two status records, the `size` line, the prompt) outruns the
/// ring before the watcher subscribes from zero (`Missing { expected: 0,
/// resumed_at: 2 }`, reproduced with 12 copies in parallel), or a burst of
/// echoes outruns its forwarder. Neither hides what it waits for: the
/// prompt and "echo: three" are each among the newest two records when
/// they appear, and the ring keeps the newest four.
async fn watch_until(watcher: &mut SessionStream, seen: &mut Vec<EventRecord>, needle: &str) {
    while !output_of(seen).contains(needle) {
        match watcher.next().await {
            Some(Ok(record)) => seen.push(record),
            Some(Err(vessel_client::StreamError::Missing { .. })) => {}
            Some(Err(error)) => panic!("{error}; before {needle:?}"),
            None => panic!("the watcher ended before {needle:?}"),
        }
    }
}

/// The ring holds the whole log: the follower gets every record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lagging_follower_catches_up_from_the_ring_when_the_store_was_pruned() {
    within(async {
        let (seen, gaps) = catch_up_after_pruning(256).await;
        assert!(gaps.is_empty(), "{gaps:?}");
        let expected: Vec<u64> = (0..u64::try_from(seen.len()).unwrap()).collect();
        assert_eq!(seqs(&seen), expected);
    })
    .await;
}

/// The ring holds only the newest records: the follower's gap ends where
/// the ring starts, not at the one record the store kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lagging_follower_skips_only_what_neither_store_nor_ring_has() {
    within(async {
        let (seen, gaps) = catch_up_after_pruning(4).await;
        // Under load the follower may subscribe only after the start-up
        // output (two status records, the size line, the prompt) has left
        // both the ring and the store: then it reports that gap first,
        // from record 0. The gap under test is the last one, from lagging.
        let Some(lag) = gaps.last() else {
            // Retention had not pruned yet when the follower read: the store
            // still had everything, and so did the follower.
            let all: Vec<u64> = (0..u64::try_from(seen.len()).unwrap()).collect();
            assert_eq!(seqs(&seen), all);
            return;
        };
        match gaps.as_slice() {
            [_lag] => {}
            [start, _lag] => assert_eq!((start.expected, start.at), (EventSeq::ZERO, 0)),
            more => panic!("more gaps than the start and the lag: {more:?}"),
        }
        // The store kept one record; the ring four. "echo: three" is at most
        // one record from the end (only "> " follows it), so a gap that ends
        // at the ring's oldest record leaves at least two records after it.
        let after = &seen[lag.at..];
        assert!(
            after.len() >= 2,
            "the gap ran to the store's last record, past the ring: {:?}",
            seqs(&seen)
        );
        let resumed: Vec<u64> = (lag.resumed_at.get()..).take(after.len()).collect();
        assert_eq!(seqs(after), resumed);
        if let Some(before) = lag.at.checked_sub(1).and_then(|last| seen.get(last)) {
            assert_eq!(before.seq.get(), lag.expected.get().checked_sub(1).unwrap());
        }
    })
    .await;
}

/// Regression: with retention pruning the store past the ring, a late
/// follower from zero used to make the node ask the ring for the same gap
/// forever. It now gets the pruned range reported once, then the ring and
/// the live records, gap-free to the end.
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
        // One line at a time, each echo awaited: every line then adds a
        // record of its own, so the log outgrows the ring of 4 however the
        // PTY chunks output (all at once, under load).
        for (line, echo) in [
            ("one\r", "echo: one"),
            ("two\r", "echo: two"),
            ("three\r", "echo: three"),
        ] {
            client.send(id, keys(line)).await.unwrap();
            // The first follower lags too; it only has to see the output.
            while !output_of(&seen).contains(echo) {
                match first.next().await.unwrap() {
                    Ok(record) => seen.push(record),
                    Err(error) => assert!(!error.is_fatal(), "{error}"),
                }
            }
        }

        // The session still runs, so the late follower starts at the ring;
        // the store keeps only the newest record.
        let late = fixture.connect(&node).await;
        let mut replay = late.follow(id, EventSeq::ZERO).await.unwrap();
        let resumed_at = match replay.next().await {
            Some(Err(vessel_client::StreamError::Missing {
                expected,
                resumed_at,
            })) => {
                assert_eq!(expected, EventSeq::ZERO);
                resumed_at
            }
            other => panic!("expected the pruned range first, got {other:?}"),
        };
        // How many records the output becomes depends on how the PTY chunks
        // it (ConPTY on Windows coalesces more than Unix PTYs), so only the
        // fact of a gap is asserted, not its size.
        assert!(resumed_at.get() > 0, "nothing was pruned: {resumed_at}");
        client.kill(id).await.unwrap();
        let mut rest = Vec::new();
        while let Some(record) = replay.next().await {
            rest.push(record.unwrap());
        }
        let expected: Vec<u64> =
            (resumed_at.get()..resumed_at.get() + u64::try_from(rest.len()).unwrap()).collect();
        assert_eq!(seqs(&rest), expected);
        assert!(matches!(
            rest.last().and_then(status_of),
            Some(SessionStatus::Exited { .. })
        ));
        node.stop().await.unwrap();
    })
    .await;
}
