//! The session runtime end to end, in terminal mode, with `fake-tui`
//! installed as `claude`.
//!
//! Tests wait on events (records, channel closure), never on time; the
//! timeout only turns a hang into a failure.

use std::num::NonZeroUsize;

use vessel_proto::{
    ErrorCode, EventSeq, HarnessKind, ModelId, SessionEvent, SessionInput, SessionMode,
    SessionSpec, SessionStatus, TerminalInput, TerminalSize,
};
use vessel_session::{
    DEFAULT_DURABLE_CAPACITY, DEFAULT_RING_CAPACITY, FollowError, RemoveError, SessionError,
};

use support::{Fixture, Follower, collect, count, keys, killed, seqs, within};

#[cfg(test)]
mod support {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::sync::broadcast::error::RecvError;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;
    use vessel_harness::Lookup;
    use vessel_harness::fakes::{Role, install};
    use vessel_proto::{
        EventRecord, EventSeq, HarnessKind, SessionEvent, SessionInput, SessionMode, SessionSpec,
        SessionStatus, TerminalBytes, TerminalInput, TerminalSize,
    };
    use vessel_session::{
        DEFAULT_HANDSHAKE_TIMEOUT, Follow, RuntimeConfig, SessionHandle, SessionRuntime,
    };
    use vessel_testkit::Fake;

    const SAFETY_NET: Duration = Duration::from_secs(30);

    /// The safety net starts after the fakes are built: the first build of
    /// a run can take a minute (see `vessel_testkit::within`).
    pub(super) async fn within<F: Future>(test: F) -> F::Output {
        vessel_testkit::within(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }

    /// A runtime whose search path holds only `fake-tui` named `claude`.
    pub(super) struct Fixture {
        pub(super) runtime: SessionRuntime,
        pub(super) shutdown: CancellationToken,
        bin: PathBuf,
    }

    impl Fixture {
        pub(super) fn new(ring_capacity: NonZeroUsize, durable_capacity: NonZeroUsize) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let bin = std::env::temp_dir()
                .join(format!("vessel-session-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&bin).unwrap();
            let tui = Fake::Tui.path().unwrap();
            drop(install(&bin, HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
            let shutdown = CancellationToken::new();
            let runtime = SessionRuntime::new(
                RuntimeConfig {
                    harness_lookup: Lookup::new([bin.clone()]),
                    ring_capacity,
                    durable_capacity,
                    agent_handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
                },
                shutdown.clone(),
            );
            Self {
                runtime,
                shutdown,
                bin,
            }
        }

        pub(super) fn dir(&self) -> &Path {
            &self.bin
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.bin));
        }
    }

    pub(super) fn terminal(harness: HarnessKind) -> SessionSpec {
        SessionSpec {
            harness,
            cwd: Some(std::env::temp_dir()),
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

    /// Drain a durable channel in the background, as the node would.
    pub(super) fn collect(
        mut durable: mpsc::Receiver<Arc<EventRecord>>,
    ) -> JoinHandle<Vec<Arc<EventRecord>>> {
        tokio::spawn(async move {
            let mut records = Vec::new();
            while let Some(record) = durable.recv().await {
                records.push(record);
            }
            records
        })
    }

    pub(super) fn seqs(records: &[Arc<EventRecord>]) -> Vec<u64> {
        records.iter().map(|record| record.seq.get()).collect()
    }

    /// The final status of a killed session. Unix reports "ended by a
    /// signal" (no code); Windows terminates the program with a code.
    pub(super) fn killed(status: &SessionStatus) -> bool {
        if cfg!(unix) {
            *status == SessionStatus::Exited { code: None }
        } else {
            matches!(status, SessionStatus::Exited { .. })
        }
    }

    pub(super) fn count(records: &[Arc<EventRecord>]) -> u64 {
        u64::try_from(records.len()).unwrap()
    }

    /// Reads one session's records from a `follow`, accumulating terminal
    /// output as text.
    pub(super) struct Follower {
        cached: VecDeque<Arc<EventRecord>>,
        follow: Follow,
        pub(super) records: Vec<Arc<EventRecord>>,
        pub(super) text: String,
    }

    impl Follower {
        pub(super) fn new(handle: &SessionHandle, from: EventSeq) -> Self {
            let follow = handle.follow(from).unwrap();
            Self {
                cached: follow.cached.iter().cloned().collect(),
                follow,
                records: Vec::new(),
                text: String::new(),
            }
        }

        /// The next record, or `None` once the session has ended.
        pub(super) async fn next(&mut self) -> Option<Arc<EventRecord>> {
            let record = match self.cached.pop_front() {
                Some(record) => record,
                None => match self.follow.live.recv().await {
                    Ok(record) => record,
                    Err(RecvError::Closed) => return None,
                    Err(RecvError::Lagged(n)) => panic!("follower lagged by {n}"),
                },
            };
            if let SessionEvent::Terminal(output) = &record.event {
                self.text
                    .push_str(&String::from_utf8_lossy(output.data.bytes()));
            }
            self.records.push(Arc::clone(&record));
            Some(record)
        }

        pub(super) async fn until_text(&mut self, needle: &str) {
            while !self.text.contains(needle) {
                assert!(
                    self.next().await.is_some(),
                    "session ended before {needle:?}; output: {:?}",
                    self.text
                );
            }
        }

        /// Read to the end; returns the final status.
        pub(super) async fn until_end(&mut self) -> SessionStatus {
            while self.next().await.is_some() {}
            match &self.records.last().unwrap().event {
                SessionEvent::Status { status } => status.clone(),
                other @ (SessionEvent::Terminal(_) | SessionEvent::Agent { .. }) => {
                    panic!("last record is not a status: {other:?}")
                }
            }
        }
    }
}

#[tokio::test]
async fn terminal_session_streams_gap_free_numbered_records() {
    within(async {
        let fixture = Fixture::new(DEFAULT_RING_CAPACITY, DEFAULT_DURABLE_CAPACITY);
        let created = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let stored = collect(created.durable);
        let handle = created.handle;
        let mut follower = Follower::new(&handle, EventSeq::ZERO);

        follower.until_text("ready").await;
        handle.send(keys("hello\r")).await.unwrap();
        follower.until_text("echo: hello").await;
        handle.send(keys("exit 4\r")).await.unwrap();
        assert_eq!(
            follower.until_end().await,
            SessionStatus::Exited { code: Some(4) }
        );

        let stored = stored.await.unwrap();
        assert_eq!(
            stored, follower.records,
            "store and followers see the same log"
        );
        assert_eq!(seqs(&stored), (0..count(&stored)).collect::<Vec<_>>());
        assert_eq!(
            stored[0].event,
            SessionEvent::Status {
                status: SessionStatus::Starting
            }
        );
        assert_eq!(
            stored[1].event,
            SessionEvent::Status {
                status: SessionStatus::Running
            }
        );

        let info = handle.info();
        assert_eq!(info.status, SessionStatus::Exited { code: Some(4) });
        assert!(info.ended_at.is_some());
        assert_eq!(info.next_seq, EventSeq::new(count(&stored)));
        assert_eq!(
            handle.send(keys("late\r")).await.unwrap_err().code(),
            ErrorCode::Conflict
        );
    })
    .await;
}

#[tokio::test]
async fn kill_stops_the_session_with_no_exit_code() {
    within(async {
        let fixture = Fixture::new(DEFAULT_RING_CAPACITY, DEFAULT_DURABLE_CAPACITY);
        let created = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let stored = collect(created.durable);
        let mut follower = Follower::new(&created.handle, EventSeq::ZERO);
        follower.until_text("ready").await;

        created.handle.kill();

        let status = follower.until_end().await;
        assert!(killed(&status), "{status:?}");
        assert_eq!(stored.await.unwrap(), follower.records);
    })
    .await;
}

#[tokio::test]
async fn shutdown_interrupts_live_sessions_and_refuses_new_ones() {
    within(async {
        let fixture = Fixture::new(DEFAULT_RING_CAPACITY, DEFAULT_DURABLE_CAPACITY);
        let created = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let stored = collect(created.durable);
        Follower::new(&created.handle, EventSeq::ZERO)
            .until_text("ready")
            .await;

        fixture.runtime.shutdown().await;

        // `shutdown` returned only after the final status was published.
        assert_eq!(created.handle.info().status, SessionStatus::Interrupted);
        let stored = stored.await.unwrap();
        assert_eq!(
            stored.last().unwrap().event,
            SessionEvent::Status {
                status: SessionStatus::Interrupted
            }
        );
        assert!(fixture.shutdown.is_cancelled());
        let refused = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap_err();
        assert!(matches!(refused, SessionError::ShuttingDown), "{refused:?}");
        assert_eq!(refused.code(), ErrorCode::Unavailable);
    })
    .await;
}

#[tokio::test]
async fn follow_reports_a_gap_older_than_the_ring() {
    within(async {
        let fixture = Fixture::new(NonZeroUsize::new(2).unwrap(), DEFAULT_DURABLE_CAPACITY);
        let created = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let stored = collect(created.durable);
        let handle = created.handle;
        handle.send(keys("exit 0\r")).await.unwrap();
        let mut watch = handle.watch_info();
        while watch.borrow_and_update().status.is_live() {
            if watch.changed().await.is_err() {
                break;
            }
        }
        let stored = stored.await.unwrap();
        // Starting, Running, at least one output chunk, Exited.
        assert!(stored.len() > 2, "enough records to overflow the ring");

        let oldest = EventSeq::new(count(&stored).saturating_sub(2));
        assert_eq!(
            handle.follow(EventSeq::ZERO).unwrap_err(),
            FollowError::Gap {
                oldest_cached: oldest
            }
        );
        let follow = handle.follow(oldest).unwrap();
        assert_eq!(follow.cached, stored[stored.len().saturating_sub(2)..]);
        assert!(
            std::sync::Arc::ptr_eq(&follow.cached[0], &stored[stored.len().saturating_sub(2)]),
            "the ring shares the records the store received"
        );
        let end = EventSeq::new(count(&stored));
        assert!(handle.follow(end).unwrap().cached.is_empty());
        assert_eq!(
            handle
                .follow(EventSeq::new(count(&stored).saturating_add(1)))
                .unwrap_err(),
            FollowError::Ahead { next: end }
        );
    })
    .await;
}

#[tokio::test]
async fn a_stalled_store_pauses_the_session_without_losing_records() {
    within(async {
        let fixture = Fixture::new(DEFAULT_RING_CAPACITY, NonZeroUsize::MIN);
        let created = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        // Nobody drains the durable channel yet: the session stalls after
        // its first record. Input still reaches the program, and kill still
        // works.
        created.handle.send(keys("hello\r")).await.unwrap();
        created.handle.kill();
        let stored = collect(created.durable).await.unwrap();

        assert_eq!(seqs(&stored), (0..count(&stored)).collect::<Vec<_>>());
        let SessionEvent::Status { status } = &stored.last().unwrap().event else {
            panic!("the last record must be the final status");
        };
        assert!(killed(status), "{status:?}");
    })
    .await;
}

#[tokio::test]
async fn input_must_match_the_session_mode() {
    within(async {
        let fixture = Fixture::new(DEFAULT_RING_CAPACITY, DEFAULT_DURABLE_CAPACITY);
        let created = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let stored = collect(created.durable);
        let handle = created.handle;

        let error = handle.send(SessionInput::CancelTurn).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unsupported);
        assert_eq!(
            error.to_string(),
            "a turn cancellation cannot be sent to a terminal session"
        );
        handle
            .send(SessionInput::Terminal {
                input: TerminalInput::Resize {
                    size: TerminalSize {
                        cols: 120,
                        rows: 40,
                    },
                },
            })
            .await
            .unwrap();

        handle.kill();
        drop(stored.await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn sessions_are_listed_newest_first_and_found_by_id() {
    within(async {
        let fixture = Fixture::new(DEFAULT_RING_CAPACITY, DEFAULT_DURABLE_CAPACITY);
        let first = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let second = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let ids: Vec<_> = fixture.runtime.list().iter().map(|info| info.id).collect();
        assert_eq!(ids, [second.handle.id(), first.handle.id()]);
        assert_eq!(
            fixture.runtime.get(first.handle.id()).unwrap().id(),
            first.handle.id()
        );

        let stored = [collect(first.durable), collect(second.durable)];
        fixture.runtime.shutdown().await;
        for stored in stored {
            drop(stored.await.unwrap());
        }
    })
    .await;
}

#[tokio::test]
async fn launch_problems_are_typed_errors() {
    within(async {
        let fixture = Fixture::new(DEFAULT_RING_CAPACITY, DEFAULT_DURABLE_CAPACITY);
        let missing = fixture
            .runtime
            .create(support::terminal(HarnessKind::Gemini))
            .await
            .unwrap_err();
        assert_eq!(missing.code(), ErrorCode::HarnessNotInstalled);

        let mut nowhere = support::terminal(HarnessKind::ClaudeCode);
        nowhere.cwd = Some(fixture.dir().join("no-such-dir"));
        let error = fixture.runtime.create(nowhere).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
        let mut relative = support::terminal(HarnessKind::ClaudeCode);
        relative.cwd = Some(std::path::PathBuf::from("."));
        let error = fixture.runtime.create(relative).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidRequest);

        let structured = SessionSpec {
            harness: HarnessKind::ClaudeCode,
            cwd: None,
            mode: SessionMode::Structured {
                model: Some(ModelId::new("cheap")),
                resume: None,
            },
        };
        let error = fixture.runtime.create(structured).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unsupported);
        assert!(fixture.runtime.list().is_empty());
    })
    .await;
}

#[tokio::test]
async fn an_ended_session_can_be_removed_and_its_holders_carry_on() {
    within(async {
        let fixture = Fixture::new(DEFAULT_RING_CAPACITY, DEFAULT_DURABLE_CAPACITY);
        let created = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let stored = collect(created.durable);
        let handle = created.handle;
        let id = handle.id();
        let mut follower = Follower::new(&handle, EventSeq::ZERO);
        follower.until_text("ready").await;

        // While it can still publish, it stays.
        assert_eq!(
            fixture.runtime.remove(id).unwrap_err(),
            RemoveError::Running(id)
        );
        assert!(fixture.runtime.get(id).is_some());

        handle.send(keys("exit 4\r")).await.unwrap();
        let ended = handle.ended().await;
        assert_eq!(ended.status, SessionStatus::Exited { code: Some(4) });
        assert_eq!(fixture.runtime.remove(id).unwrap(), ended);
        assert!(fixture.runtime.get(id).is_none());
        assert!(fixture.runtime.list().iter().all(|info| info.id != id));
        assert_eq!(
            fixture.runtime.remove(id).unwrap_err(),
            RemoveError::NotFound(id)
        );

        // A follower from before the removal gets every record, then the end.
        assert_eq!(
            follower.until_end().await,
            SessionStatus::Exited { code: Some(4) }
        );
        let stored = stored.await.unwrap();
        assert_eq!(stored, follower.records);

        // A handle held across the removal still answers, and can still
        // replay the ring.
        assert_eq!(handle.info(), ended);
        assert_eq!(handle.ended().await, ended);
        assert_eq!(
            handle.send(keys("late\r")).await.unwrap_err().code(),
            ErrorCode::Conflict
        );
        handle.kill();
        let mut late = Follower::new(&handle, EventSeq::ZERO);
        assert_eq!(
            late.until_end().await,
            SessionStatus::Exited { code: Some(4) }
        );
        assert_eq!(late.records, stored);
    })
    .await;
}

/// A process the program started in a group of its own survives the group
/// kill and keeps the PTY open. The session must still end, within the
/// drain deadline, with the program's own exit code.
#[cfg(unix)]
#[tokio::test]
async fn a_process_that_escaped_the_group_does_not_keep_the_session_open() {
    within(async {
        let fixture = Fixture::new(DEFAULT_RING_CAPACITY, DEFAULT_DURABLE_CAPACITY);
        let created = fixture
            .runtime
            .create(support::terminal(HarnessKind::ClaudeCode))
            .await
            .unwrap();
        let stored = collect(created.durable);
        let handle = created.handle;
        let mut follower = Follower::new(&handle, EventSeq::ZERO);
        follower.until_text("ready").await;
        handle.send(keys("escape\r")).await.unwrap();
        follower.until_text("escaped ").await;
        follower.until_text("\n").await;
        let escapee: u32 = follower
            .text
            .lines()
            .find_map(|line| line.trim().strip_prefix("escaped "))
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        handle.send(keys("exit 3\r")).await.unwrap();
        assert_eq!(
            follower.until_end().await,
            SessionStatus::Exited { code: Some(3) }
        );
        drop(stored.await.unwrap());

        // The escapee is still there (that was the point); remove it.
        let killed = std::process::Command::new("kill")
            .args(["-9", &escapee.to_string()])
            .status()
            .unwrap();
        assert!(killed.success(), "the escaped process should still run");
    })
    .await;
}
