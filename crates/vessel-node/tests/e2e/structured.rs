//! Structured mode: `fake-acp` as Claude's ACP adapter. Two clients share
//! one conversation; the first permission answer wins; the node restarts
//! and the conversation resumes through its harness session id.

use vessel_client::{NodeClient, SessionStream};
use vessel_node::Node;
use vessel_proto::{
    AgentEvent, ContentBlock, ErrorCode, EventRecord, EventSeq, HarnessKind, HarnessSessionId,
    PermissionId, PermissionOptionId, PermissionOutcome, SessionId, SessionInput, SessionStatus,
    StopReason, StructuredSupport, ToolCallStatus,
};

use crate::support::{
    Fixture, agent_of, keys, prompt, read_all, read_until, seqs, status_of, structured_spec, within,
};

fn answer(id: PermissionId, option: &str) -> SessionInput {
    SessionInput::AnswerPermission {
        id,
        outcome: PermissionOutcome::Selected {
            option: PermissionOptionId::new(option),
        },
    }
}

fn turn_ended(reason: StopReason) -> impl Fn(&EventRecord) -> bool {
    move |record| matches!(agent_of(record), Some(AgentEvent::TurnEnded { reason: got }) if *got == reason)
}

fn running(record: &EventRecord) -> bool {
    status_of(record) == Some(&SessionStatus::Running)
}

/// Whether `record` holds an agent reply chunk satisfying `wanted`.
fn reply(record: &EventRecord, wanted: impl Fn(&str) -> bool) -> bool {
    matches!(
        agent_of(record),
        Some(AgentEvent::MessageChunk { content: ContentBlock::Text { text } }) if wanted(text)
    )
}

/// One structured session followed by two clients.
struct Shared {
    id: SessionId,
    alice: NodeClient,
    bob: NodeClient,
    alice_view: SessionStream,
    bob_view: SessionStream,
    seen: Vec<EventRecord>,
    bob_seen: Vec<EventRecord>,
}

impl Shared {
    /// Start a session and wait until it runs; returns it with the harness
    /// session id from the agent's `Ready`.
    async fn open(fixture: &Fixture, node: &Node) -> (Self, HarnessSessionId) {
        let alice = fixture.connect(node).await;
        let bob = fixture.connect(node).await;
        let claude = alice
            .harnesses()
            .await
            .unwrap()
            .into_iter()
            .find(|harness| harness.kind == HarnessKind::ClaudeCode)
            .unwrap();
        assert_eq!(claude.structured, StructuredSupport::Available);

        let id = alice
            .create_session(structured_spec(fixture, None))
            .await
            .unwrap()
            .id;
        let mut shared = Self {
            id,
            alice_view: alice.follow(id, EventSeq::ZERO).await.unwrap(),
            bob_view: bob.follow(id, EventSeq::ZERO).await.unwrap(),
            alice,
            bob,
            seen: Vec::new(),
            bob_seen: Vec::new(),
        };
        // Starting, then the agent's Ready, then Running.
        read_until(&mut shared.alice_view, &mut shared.seen, running).await;
        let ready = shared
            .seen
            .iter()
            .find_map(|record| match agent_of(record) {
                Some(AgentEvent::Ready { harness_session }) => Some(harness_session.clone()),
                _other => None,
            })
            .unwrap();
        (shared, ready)
    }

    /// A full turn whose permission request both clients see; Bob answers
    /// first and Alice's late answer is refused.
    async fn permission_turn(&mut self) {
        let id = self.id;
        self.alice.send(id, prompt("hello vessel")).await.unwrap();
        let requested = |record: &EventRecord| {
            matches!(agent_of(record), Some(AgentEvent::PermissionRequested(_)))
        };
        read_until(&mut self.alice_view, &mut self.seen, requested).await;
        let Some(AgentEvent::PermissionRequested(request)) =
            self.seen.last().and_then(agent_of).cloned()
        else {
            panic!("not a permission request");
        };
        read_until(&mut self.bob_view, &mut self.bob_seen, requested).await;
        assert!(self.seen.iter().any(|record| reply(record, |text| {
            text.contains("echo: hello vessel (model fake-large)")
        })));

        self.bob
            .send(id, answer(request.id, "allow-once"))
            .await
            .unwrap();
        let late = self.alice.send(id, answer(request.id, "reject-once")).await;
        assert_eq!(late.unwrap_err().code(), Some(ErrorCode::Conflict));
        read_until(
            &mut self.alice_view,
            &mut self.seen,
            turn_ended(StopReason::EndTurn),
        )
        .await;
        assert!(self.seen.iter().any(|record| matches!(
            agent_of(record),
            Some(AgentEvent::PermissionResolved {
                id: resolved,
                outcome: PermissionOutcome::Selected { option },
            }) if *resolved == request.id && option.as_str() == "allow-once"
        )));
        assert!(self.seen.iter().any(|record| matches!(
            agent_of(record),
            Some(AgentEvent::ToolCallUpdate(update))
                if update.status == Some(ToolCallStatus::Completed)
        )));
    }

    /// Wrong answers and wrong input carry their own codes.
    async fn refusals(&self) {
        let unknown = self
            .alice
            .send(self.id, answer(PermissionId::random(), "allow-once"))
            .await;
        assert_eq!(unknown.unwrap_err().code(), Some(ErrorCode::NotFound));
        let typed = self.alice.send(self.id, keys("ls\r")).await;
        assert_eq!(typed.unwrap_err().code(), Some(ErrorCode::Unsupported));
    }

    /// A turn that waits until cancelled; afterwards Bob has seen exactly
    /// what Alice saw, gap-free.
    async fn cancelled_turn(&mut self) {
        self.alice.send(self.id, prompt("wait")).await.unwrap();
        read_until(&mut self.alice_view, &mut self.seen, |record| {
            started_with(record, "wait")
        })
        .await;
        self.alice
            .send(self.id, SessionInput::CancelTurn)
            .await
            .unwrap();
        let cancelled = turn_ended(StopReason::Cancelled);
        read_until(&mut self.alice_view, &mut self.seen, &cancelled).await;
        read_until(&mut self.bob_view, &mut self.bob_seen, &cancelled).await;
        assert_eq!(self.bob_seen, self.seen[..self.bob_seen.len()]);
        let expected: Vec<u64> = (0..u64::try_from(self.seen.len()).unwrap()).collect();
        assert_eq!(seqs(&self.seen), expected);
    }

    /// After the node stopped: Alice's stream ends with `Interrupted`.
    async fn interrupted(mut self) -> Vec<EventRecord> {
        let mut tail = Vec::new();
        while let Some(record) = self.alice_view.next().await {
            tail.push(record.unwrap());
        }
        assert_eq!(
            tail.last().and_then(status_of),
            Some(&SessionStatus::Interrupted)
        );
        self.seen
    }
}

/// Whether `record` starts a turn with exactly `text`.
fn started_with(record: &EventRecord, text: &str) -> bool {
    matches!(
        agent_of(record),
        Some(AgentEvent::TurnStarted { content })
            if content == &[ContentBlock::Text { text: text.to_owned() }]
    )
}

/// After a restart: the session is interrupted with its log and harness
/// session id intact, and resuming through that id replays history and
/// takes new prompts.
async fn restart_and_resume(
    fixture: &Fixture,
    id: SessionId,
    ready: &HarnessSessionId,
    seen: &[EventRecord],
) {
    let node = fixture.start().await;
    let client = fixture.connect(&node).await;
    let restored = client.session(id).await.unwrap();
    assert_eq!(restored.status, SessionStatus::Interrupted);
    assert_eq!(restored.harness_session.as_ref(), Some(ready));
    let log = read_all(&client, id).await;
    assert_eq!(log[..seen.len()], seen[..]);
    assert_eq!(
        log.last().and_then(status_of),
        Some(&SessionStatus::Interrupted)
    );

    let resumed = client
        .create_session(structured_spec(fixture, Some(ready.clone())))
        .await
        .unwrap();
    let mut view = client.follow(resumed.id, EventSeq::ZERO).await.unwrap();
    let mut resumed_seen = Vec::new();
    let replayed = format!("replayed {ready}");
    read_until(&mut view, &mut resumed_seen, |record| {
        reply(record, |text| text.contains(&replayed))
    })
    .await;
    read_until(&mut view, &mut resumed_seen, running).await;
    client.send(resumed.id, prompt("plain")).await.unwrap();
    read_until(
        &mut view,
        &mut resumed_seen,
        turn_ended(StopReason::EndTurn),
    )
    .await;
    assert!(
        resumed_seen
            .iter()
            .any(|record| reply(record, |text| text == "echo: plain"))
    );
    node.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_structured_session_is_shared_answered_interrupted_and_resumed() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start().await;
        let (mut shared, ready) = Shared::open(&fixture, &node).await;
        let id = shared.id;
        shared.permission_turn().await;
        shared.refusals().await;
        shared.cancelled_turn().await;
        node.stop().await.unwrap();
        let seen = shared.interrupted().await;
        restart_and_resume(&fixture, id, &ready, &seen).await;
    })
    .await;
}
