//! The ACP client against the `fake-acp` testkit agent.
//!
//! Tests wait on outputs, never on time; the timeout only turns a hang into
//! a failure.

use vessel_agent::{AgentSetup, Disconnect, InputError, Signal};
use vessel_proto::{
    AgentEvent, ContentBlock, HarnessSessionId, ModelId, PermissionOptionId, PermissionOutcome,
    StopReason, ToolCallId, ToolCallStatus,
};

use support::{Agent, setup, text, within};

#[cfg(test)]
mod support {
    use std::future::Future;
    use std::time::Duration;

    use vessel_agent::{
        AgentInput, AgentOutput, AgentOutputs, AgentProcess, AgentSetup, Disconnect,
    };
    use vessel_proc::CommandSpec;
    use vessel_proto::{AgentEvent, ContentBlock, ModelId};
    use vessel_testkit::Fake;

    const SAFETY_NET: Duration = Duration::from_secs(30);

    /// The safety net starts after the fakes are built: the first build of
    /// a run can take a minute (see `vessel_testkit::within`).
    pub(super) async fn within<F: Future>(test: F) -> F::Output {
        vessel_testkit::within(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }

    pub(super) fn setup(model: Option<&str>) -> AgentSetup {
        AgentSetup {
            cwd: std::env::temp_dir(),
            model: model.map(ModelId::new),
            resume: None,
            handshake_timeout: vessel_agent::DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }

    pub(super) fn text(text: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }]
    }

    /// A running `fake-acp` and everything it reported so far.
    pub(super) struct Agent {
        pub(super) process: AgentProcess,
        pub(super) input: AgentInput,
        outputs: AgentOutputs,
        /// Timeline events, harness log lines excluded.
        pub(super) events: Vec<AgentEvent>,
        pub(super) log: Vec<String>,
        pub(super) disconnect: Option<Disconnect>,
    }

    impl Agent {
        pub(super) fn start(setup: AgentSetup) -> Self {
            Self::start_with(&CommandSpec::new(Fake::Acp.path().unwrap()), setup)
        }

        pub(super) fn start_with(command: &CommandSpec, setup: AgentSetup) -> Self {
            let (process, input, outputs) = AgentProcess::spawn(command, setup).unwrap();
            Self {
                process,
                input,
                outputs,
                events: Vec::new(),
                log: Vec::new(),
                disconnect: None,
            }
        }

        /// The next timeline event, or `None` once the outputs ended.
        pub(super) async fn next(&mut self) -> Option<AgentEvent> {
            loop {
                match self.outputs.next().await? {
                    AgentOutput::Event(AgentEvent::HarnessLog { line }) => self.log.push(line),
                    AgentOutput::Event(event) => {
                        self.events.push(event.clone());
                        return Some(event);
                    }
                    AgentOutput::Disconnected(disconnect) => {
                        self.disconnect = Some(disconnect);
                    }
                }
            }
        }

        /// Read events until one matches; returns it.
        pub(super) async fn until(&mut self, wanted: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
            loop {
                let Some(event) = self.next().await else {
                    panic!(
                        "outputs ended first; events {:?}, log {:?}, disconnect {:?}",
                        self.events, self.log, self.disconnect
                    );
                };
                if wanted(&event) {
                    return event;
                }
            }
        }

        /// Read outputs until the connection ends (the process may live on);
        /// returns why it ended.
        pub(super) async fn disconnected(&mut self) -> Disconnect {
            loop {
                match self
                    .outputs
                    .next()
                    .await
                    .expect("outputs ended before the disconnect")
                {
                    AgentOutput::Event(AgentEvent::HarnessLog { line }) => self.log.push(line),
                    AgentOutput::Event(event) => self.events.push(event),
                    AgentOutput::Disconnected(disconnect) => {
                        self.disconnect = Some(disconnect.clone());
                        return disconnect;
                    }
                }
            }
        }

        /// Read to the end of the outputs; returns why the connection ended.
        pub(super) async fn until_end(&mut self) -> Disconnect {
            while self.next().await.is_some() {}
            self.disconnect
                .clone()
                .expect("the driver always reports a disconnect")
        }
    }
}

const fn ready(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::Ready { .. })
}

const fn turn_over(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::TurnEnded { .. } | AgentEvent::TurnFailed { .. }
    )
}

#[tokio::test]
async fn a_full_turn_with_a_permission_round_trip() {
    within(async {
        let mut agent = Agent::start(setup(Some("fake-large")));
        assert_eq!(
            agent.until(ready).await,
            AgentEvent::Ready {
                harness_session: HarnessSessionId::new("fake-session-1")
            }
        );

        agent.input.prompt(text("hello")).await.unwrap();
        let AgentEvent::PermissionRequested(request) = agent
            .until(|event| matches!(event, AgentEvent::PermissionRequested(_)))
            .await
        else {
            panic!("matched by the predicate above")
        };
        assert_eq!(request.tool_call, ToolCallId::new("call-1"));
        let options: Vec<_> = request
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect();
        assert_eq!(options, ["allow-once", "reject-once"]);

        let allow = PermissionOutcome::Selected {
            option: PermissionOptionId::new("allow-once"),
        };
        agent.input.answer(request.id, allow.clone()).await.unwrap();
        let late = agent
            .input
            .answer(request.id, allow.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(late, InputError::PermissionSettled(id) if id == request.id),
            "{late:?}"
        );
        assert_eq!(
            agent.until(turn_over).await,
            AgentEvent::TurnEnded {
                reason: StopReason::EndTurn
            }
        );

        let kinds: Vec<&str> = agent.events.iter().map(kind).collect();
        assert_eq!(
            kinds,
            [
                "ready",
                "turn_started",
                "thought",
                "message",
                "tool_call",
                "plan",
                "tool_call_update",
                "permission_requested",
                "permission_resolved",
                "tool_call_update",
                "turn_ended"
            ]
        );
        assert!(agent.events.contains(&AgentEvent::MessageChunk {
            content: ContentBlock::Text {
                text: "echo: hello (model fake-large)".to_owned()
            }
        }));
        let AgentEvent::ToolCall(call) = &agent.events[4] else {
            panic!("matched by the predicate above")
        };
        assert_eq!(
            call.raw_input.as_ref().unwrap().as_text(),
            r#"{"command":"fake","args":["--fast"]}"#
        );
        let AgentEvent::ToolCallUpdate(done) = &agent.events[9] else {
            panic!("matched by the predicate above")
        };
        assert_eq!(done.status, Some(ToolCallStatus::Completed));
        assert_eq!(
            done.raw_output.as_ref().unwrap().as_text(),
            r#"{"ok":true}"#
        );
        assert!(
            agent.log.iter().any(|line| line == "fake-acp starting"),
            "{:?}",
            agent.log
        );

        agent.process.signal(Signal::Terminate).unwrap();
        assert_eq!(agent.until_end().await, Disconnect::Closed);
        let _ = agent.process.wait().await;
    })
    .await;
}

fn kind(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::Ready { .. } => "ready",
        AgentEvent::TurnStarted { .. } => "turn_started",
        AgentEvent::UserMessageChunk { .. } => "user_message",
        AgentEvent::MessageChunk { .. } => "message",
        AgentEvent::ThoughtChunk { .. } => "thought",
        AgentEvent::ToolCall(_) => "tool_call",
        AgentEvent::ToolCallUpdate(_) => "tool_call_update",
        AgentEvent::Plan { .. } => "plan",
        AgentEvent::PermissionRequested(_) => "permission_requested",
        AgentEvent::PermissionResolved { .. } => "permission_resolved",
        AgentEvent::TurnEnded { .. } => "turn_ended",
        AgentEvent::TurnFailed { .. } => "turn_failed",
        AgentEvent::HarnessLog { .. } => "log",
    }
}

#[tokio::test]
async fn a_rejected_tool_fails_and_bad_answers_are_refused() {
    within(async {
        let mut agent = Agent::start(setup(None));
        drop(agent.until(ready).await);
        agent.input.prompt(text("hello")).await.unwrap();
        let busy = agent.input.prompt(text("again")).await.unwrap_err();
        assert!(matches!(busy, InputError::TurnInProgress), "{busy:?}");

        let AgentEvent::PermissionRequested(request) = agent
            .until(|event| matches!(event, AgentEvent::PermissionRequested(_)))
            .await
        else {
            panic!("matched by the predicate above")
        };
        let unknown = agent
            .input
            .answer(
                request.id,
                PermissionOutcome::Selected {
                    option: PermissionOptionId::new("allow-forever"),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(unknown, InputError::UnknownOption { .. }), "{unknown:?}");
        let missing = agent
            .input
            .answer(vessel_proto::PermissionId::random(), PermissionOutcome::Cancelled)
            .await
            .unwrap_err();
        assert!(matches!(missing, InputError::PermissionNotFound(_)), "{missing:?}");

        agent
            .input
            .answer(
                request.id,
                PermissionOutcome::Selected {
                    option: PermissionOptionId::new("reject-once"),
                },
            )
            .await
            .unwrap();
        drop(agent.until(turn_over).await);
        let failed = agent.events.iter().any(|event| {
            matches!(event, AgentEvent::ToolCallUpdate(update) if update.status == Some(ToolCallStatus::Failed))
        });
        assert!(failed, "{:?}", agent.events);
        assert!(
            agent.events.contains(&AgentEvent::MessageChunk {
                content: ContentBlock::Text {
                    text: "echo: hello (model fake-small)".to_owned()
                }
            }),
            "the default model stays when none is requested"
        );
    })
    .await;
}

#[tokio::test]
async fn cancelling_withdraws_pending_permissions_and_ends_the_turn() {
    within(async {
        let mut agent = Agent::start(setup(None));
        drop(agent.until(ready).await);

        agent.input.prompt(text("hello")).await.unwrap();
        let AgentEvent::PermissionRequested(request) = agent
            .until(|event| matches!(event, AgentEvent::PermissionRequested(_)))
            .await
        else {
            panic!("matched by the predicate above")
        };
        agent.input.cancel_turn().await.unwrap();
        assert_eq!(
            agent
                .until(|event| matches!(event, AgentEvent::PermissionResolved { .. }))
                .await,
            AgentEvent::PermissionResolved {
                id: request.id,
                outcome: PermissionOutcome::Cancelled
            }
        );
        assert_eq!(
            agent.until(turn_over).await,
            AgentEvent::TurnEnded {
                reason: StopReason::Cancelled
            }
        );

        // A turn with no permission request, cancelled while it runs.
        agent.input.prompt(text("wait")).await.unwrap();
        agent.input.cancel_turn().await.unwrap();
        assert_eq!(
            agent.until(turn_over).await,
            AgentEvent::TurnEnded {
                reason: StopReason::Cancelled
            }
        );
        // Cancelling with no turn running is a no-op.
        agent.input.cancel_turn().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn resuming_loads_the_session_and_replays_its_history() {
    within(async {
        let mut agent = Agent::start(AgentSetup {
            resume: Some(HarnessSessionId::new("fake-session-9")),
            ..setup(None)
        });
        // The replay arrives before the handshake completes.
        assert_eq!(
            agent.next().await,
            Some(AgentEvent::MessageChunk {
                content: ContentBlock::Text {
                    text: "replayed fake-session-9".to_owned()
                }
            })
        );
        assert_eq!(
            agent.next().await,
            Some(AgentEvent::Ready {
                harness_session: HarnessSessionId::new("fake-session-9")
            })
        );
        agent.input.prompt(text("plain")).await.unwrap();
        drop(agent.until(turn_over).await);
    })
    .await;
}

#[tokio::test]
async fn a_model_the_agent_does_not_offer_fails_the_handshake() {
    within(async {
        let mut agent = Agent::start(AgentSetup {
            model: Some(ModelId::new("gpt-imaginary")),
            ..setup(None)
        });
        let Disconnect::Handshake { message } = agent.until_end().await else {
            panic!("expected a handshake failure");
        };
        assert!(
            message.contains("does not offer model `gpt-imaginary`"),
            "{message}"
        );
        assert!(agent.events.is_empty(), "never ready: {:?}", agent.events);
        let error = agent.input.prompt(text("hello")).await.unwrap_err();
        assert!(matches!(error, InputError::Closed), "{error:?}");
    })
    .await;
}

#[tokio::test]
async fn an_agent_that_dies_at_start_reports_its_stderr_and_exit_code() {
    within(async {
        let command = vessel_proc::CommandSpec::new(vessel_testkit::Fake::Acp.path().unwrap())
            .env("FAKE_ACP_EXIT_EARLY", "1");
        let mut agent = Agent::start_with(&command, setup(None));
        // Which way the connection ends is a race: the agent may be gone
        // before `initialize` is written (a broken pipe, `Protocol`), while
        // it is read (`Handshake`), or after (`Closed`). Any of them means
        // the session never became ready.
        let disconnect = agent.until_end().await;
        assert!(
            agent.events.is_empty(),
            "{disconnect:?}: {:?}",
            agent.events
        );
        assert_eq!(agent.log, ["fake-acp: not logged in"]);
        assert_eq!(agent.process.wait().await, Some(3));
    })
    .await;
}

/// An adapter stuck at start fails the handshake after the deadline, so
/// the session does not stay `Starting` and input is refused, not held.
#[tokio::test]
async fn an_agent_stuck_in_the_handshake_fails_at_the_deadline() {
    within(async {
        for step in ["initialize", "session/new"] {
            let command = vessel_proc::CommandSpec::new(vessel_testkit::Fake::Acp.path().unwrap())
                .env("FAKE_ACP_HANG", step);
            let mut agent = Agent::start_with(
                &command,
                AgentSetup {
                    handshake_timeout: std::time::Duration::from_millis(300),
                    ..setup(None)
                },
            );
            let Disconnect::Handshake { message } = agent.disconnected().await else {
                panic!("a stuck {step} must fail the handshake");
            };
            assert!(
                message.contains("did not complete the ACP handshake within"),
                "{step}: {message}"
            );
            assert!(agent.events.is_empty(), "never ready: {:?}", agent.events);
            let error = agent.input.prompt(text("hello")).await.unwrap_err();
            assert!(matches!(error, InputError::Closed), "{error:?}");
            // The process is stuck too; stopping it (the session's job) ends
            // the outputs.
            agent.process.signal(Signal::Kill).unwrap();
            drop(agent.until_end().await);
        }
    })
    .await;
}
