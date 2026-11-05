//! The session runtime end to end, in structured mode, with `fake-acp`
//! installed as `claude-agent-acp`.
//!
//! Tests wait on events, never on time; the timeout only turns a hang into
//! a failure.

use vessel_proto::{
    AgentEvent, ContentBlock, ErrorCode, HarnessKind, HarnessSessionId, ModelId,
    PermissionOptionId, PermissionOutcome, SessionEvent, SessionInput, SessionMode, SessionSpec,
    SessionStatus, StopReason, TerminalInput, TerminalSize,
};

use support::{Fixture, Follower, collect, within};

#[cfg(test)]
mod support {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use tokio::sync::broadcast::error::RecvError;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;
    use vessel_harness::Lookup;
    use vessel_harness::fakes::{Role, install};
    use vessel_proto::{EventRecord, EventSeq, HarnessKind, SessionEvent, SessionStatus};
    use vessel_session::{
        DEFAULT_DURABLE_CAPACITY, DEFAULT_RING_CAPACITY, Follow, RuntimeConfig, SessionHandle,
        SessionRuntime,
    };
    use vessel_testkit::Fake;

    const SAFETY_NET: Duration = Duration::from_secs(30);

    pub(super) async fn within<F: Future>(test: F) -> F::Output {
        tokio::time::timeout(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }

    /// A runtime whose search path holds `fake-acp` as Claude's adapter.
    pub(super) struct Fixture {
        pub(super) runtime: SessionRuntime,
        bin: PathBuf,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let bin = std::env::temp_dir()
                .join(format!("vessel-structured-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&bin).unwrap();
            let acp = Fake::Acp.path().unwrap();
            drop(install(&bin, HarnessKind::ClaudeCode, Role::Structured, &acp).unwrap());
            let runtime = SessionRuntime::new(
                RuntimeConfig {
                    harness_lookup: Lookup::new([bin.clone()]),
                    ring_capacity: DEFAULT_RING_CAPACITY,
                    durable_capacity: DEFAULT_DURABLE_CAPACITY,
                },
                CancellationToken::new(),
            );
            Self { runtime, bin }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.bin));
        }
    }

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

    /// Reads one session's records from the start.
    pub(super) struct Follower {
        cached: VecDeque<Arc<EventRecord>>,
        follow: Follow,
        pub(super) records: Vec<Arc<EventRecord>>,
    }

    impl Follower {
        pub(super) fn new(handle: &SessionHandle) -> Self {
            let follow = handle.follow(EventSeq::ZERO).unwrap();
            Self {
                cached: follow.cached.iter().cloned().collect(),
                follow,
                records: Vec::new(),
            }
        }

        pub(super) async fn next(&mut self) -> Option<SessionEvent> {
            let record = match self.cached.pop_front() {
                Some(record) => record,
                None => match self.follow.live.recv().await {
                    Ok(record) => record,
                    Err(RecvError::Closed) => return None,
                    Err(RecvError::Lagged(n)) => panic!("follower lagged by {n}"),
                },
            };
            self.records.push(Arc::clone(&record));
            Some(record.event.clone())
        }

        pub(super) async fn until(
            &mut self,
            wanted: impl Fn(&SessionEvent) -> bool,
        ) -> SessionEvent {
            loop {
                let Some(event) = self.next().await else {
                    panic!("session ended first: {:?}", self.records);
                };
                if wanted(&event) {
                    return event;
                }
            }
        }

        /// Read to the end; returns the final status.
        pub(super) async fn until_end(&mut self) -> SessionStatus {
            let mut last = None;
            while let Some(event) = self.next().await {
                if let SessionEvent::Status { status } = event {
                    last = Some(status);
                }
            }
            last.expect("every session ends with a status")
        }
    }
}

fn structured(model: Option<&str>) -> SessionSpec {
    SessionSpec {
        harness: HarnessKind::ClaudeCode,
        cwd: Some(std::env::temp_dir()),
        mode: SessionMode::Structured {
            model: model.map(ModelId::new),
            resume: None,
        },
    }
}

fn prompt(text: &str) -> SessionInput {
    SessionInput::Prompt {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    }
}

fn agent(wanted: fn(&AgentEvent) -> bool) -> impl Fn(&SessionEvent) -> bool {
    move |event| matches!(event, SessionEvent::Agent { event } if wanted(event))
}

const fn status(event: &SessionEvent) -> Option<&SessionStatus> {
    match event {
        SessionEvent::Status { status } => Some(status),
        SessionEvent::Terminal(_) | SessionEvent::Agent { .. } => None,
    }
}

#[tokio::test]
async fn status_follows_the_acp_handshake() {
    within(async {
        let fixture = Fixture::new();
        let created = fixture
            .runtime
            .create(structured(Some("fake-large")))
            .await
            .unwrap();
        let stored = collect(created.durable);
        let handle = created.handle;
        let mut follower = Follower::new(&handle);

        let running = follower
            .until(|event| status(event) == Some(&SessionStatus::Running))
            .await;
        assert_eq!(
            running,
            SessionEvent::Status {
                status: SessionStatus::Running
            }
        );
        let first: Vec<_> = follower
            .records
            .iter()
            .filter(|record| {
                !matches!(
                    &record.event,
                    SessionEvent::Agent {
                        event: AgentEvent::HarnessLog { .. }
                    }
                )
            })
            .map(|record| record.event.clone())
            .collect();
        assert_eq!(
            first,
            [
                SessionEvent::Status {
                    status: SessionStatus::Starting
                },
                SessionEvent::Agent {
                    event: AgentEvent::Ready {
                        harness_session: HarnessSessionId::new("fake-session-1")
                    }
                },
                SessionEvent::Status {
                    status: SessionStatus::Running
                },
            ]
        );
        assert_eq!(
            handle.info().harness_session,
            Some(HarnessSessionId::new("fake-session-1"))
        );

        handle.kill();
        let end = follower.until_end().await;
        assert!(matches!(end, SessionStatus::Exited { .. }), "{end:?}");
        assert_eq!(stored.await.unwrap(), follower.records);
    })
    .await;
}

#[tokio::test]
async fn a_turn_with_a_permission_round_trip() {
    within(async {
        let fixture = Fixture::new();
        let created = fixture
            .runtime
            .create(structured(Some("fake-large")))
            .await
            .unwrap();
        let stored = collect(created.durable);
        let handle = created.handle;
        let mut follower = Follower::new(&handle);

        handle.send(prompt("hello")).await.unwrap();
        let SessionEvent::Agent {
            event: AgentEvent::PermissionRequested(request),
        } = follower
            .until(agent(|event| {
                matches!(event, AgentEvent::PermissionRequested(_))
            }))
            .await
        else {
            panic!("matched by the predicate above");
        };
        let allow = SessionInput::AnswerPermission {
            id: request.id,
            outcome: PermissionOutcome::Selected {
                option: PermissionOptionId::new("allow-once"),
            },
        };
        handle.send(allow.clone()).await.unwrap();
        assert_eq!(
            handle.send(allow).await.unwrap_err().code(),
            ErrorCode::Conflict
        );
        assert_eq!(
            follower
                .until(agent(|event| matches!(event, AgentEvent::TurnEnded { .. })))
                .await,
            SessionEvent::Agent {
                event: AgentEvent::TurnEnded {
                    reason: StopReason::EndTurn
                }
            }
        );
        assert!(follower.records.iter().any(|record| record.event
            == SessionEvent::Agent {
                event: AgentEvent::MessageChunk {
                    content: ContentBlock::Text {
                        text: "echo: hello (model fake-large)".to_owned()
                    }
                }
            }));

        let keys = SessionInput::Terminal {
            input: TerminalInput::Resize {
                size: TerminalSize::default(),
            },
        };
        assert_eq!(
            handle.send(keys).await.unwrap_err().code(),
            ErrorCode::Unsupported
        );

        handle.kill();
        let end = follower.until_end().await;
        assert!(matches!(end, SessionStatus::Exited { .. }), "{end:?}");
        assert_eq!(stored.await.unwrap(), follower.records);
    })
    .await;
}

#[tokio::test]
async fn prompts_sent_while_starting_wait_for_the_handshake() {
    within(async {
        let fixture = Fixture::new();
        let created = fixture.runtime.create(structured(None)).await.unwrap();
        let stored = collect(created.durable);
        let handle = created.handle;
        // Sent before the agent is ready: it waits, then runs.
        handle.send(prompt("plain")).await.unwrap();
        let mut follower = Follower::new(&handle);
        drop(
            follower
                .until(agent(|event| matches!(event, AgentEvent::TurnEnded { .. })))
                .await,
        );
        handle.kill();
        drop(follower.until_end().await);
        drop(stored.await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn a_model_the_agent_does_not_offer_fails_the_session() {
    within(async {
        let fixture = Fixture::new();
        let created = fixture
            .runtime
            .create(structured(Some("gpt-imaginary")))
            .await
            .unwrap();
        let stored = collect(created.durable);
        let mut follower = Follower::new(&created.handle);
        let SessionStatus::Failed { message } = follower.until_end().await else {
            panic!("a failed handshake fails the session");
        };
        assert!(
            message.contains("does not offer model `gpt-imaginary`"),
            "{message}"
        );
        assert!(
            !follower
                .records
                .iter()
                .any(|record| status(&record.event) == Some(&SessionStatus::Running)),
            "never running"
        );
        assert_eq!(stored.await.unwrap(), follower.records);
        assert!(matches!(
            created.handle.info().status,
            SessionStatus::Failed { .. }
        ));
    })
    .await;
}

#[tokio::test]
async fn resuming_replays_history_before_the_session_runs() {
    within(async {
        let fixture = Fixture::new();
        let mut spec = structured(None);
        spec.mode = SessionMode::Structured {
            model: None,
            resume: Some(HarnessSessionId::new("fake-session-7")),
        };
        let created = fixture.runtime.create(spec).await.unwrap();
        let stored = collect(created.durable);
        let mut follower = Follower::new(&created.handle);
        drop(
            follower
                .until(|event| status(event) == Some(&SessionStatus::Running))
                .await,
        );
        let replay = SessionEvent::Agent {
            event: AgentEvent::MessageChunk {
                content: ContentBlock::Text {
                    text: "replayed fake-session-7".to_owned(),
                },
            },
        };
        assert!(follower.records.iter().any(|record| record.event == replay));
        assert_eq!(
            created.handle.info().harness_session,
            Some(HarnessSessionId::new("fake-session-7"))
        );
        fixture.runtime.shutdown().await;
        assert_eq!(follower.until_end().await, SessionStatus::Interrupted);
        drop(stored.await.unwrap());
    })
    .await;
}
