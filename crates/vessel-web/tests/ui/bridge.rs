//! The bridge against real nodes in this process: calls in and replies
//! out, refusals answered by id, one connection per node for every tab,
//! and a followed session that survives its node's connection dropping.
#![expect(
    clippy::wildcard_enum_match_arm,
    reason = "tests reading the few frames they wait for among the others"
)]

use std::sync::atomic::Ordering;

use bytes::Bytes;
use vessel_proto::{
    AgentEvent, Call, ContentBlock, CreateSession, ErrorCode, EventRecord, EventSeq, GetNode,
    GetSession, HarnessKind, ListSessions, PermissionOptionId, PermissionOutcome, Reply, SendInput,
    SessionEvent, SessionId, SessionInput, SessionMode, SessionSpec, TerminalBytes, TerminalInput,
    TerminalOutput, TerminalSize,
};
use vessel_web::wire::{
    CallId, CallOutcome, ClientFrame, Failure, NodeKey, NodeStatus, Op, OpOutcome, OpResult,
    ServerFrame, StreamId, StreamState,
};

use crate::support::{Proxy, Source, Tab, TestNode, Ui, within};

/// The answer to call `id`.
async fn reply(tab: &mut Tab, id: CallId) -> CallOutcome {
    tab.until(|frame| match frame {
        ServerFrame::Reply {
            id: replied,
            outcome,
        } if replied == id => Some(outcome),
        _other => None,
    })
    .await
}

/// The answer to op `id`.
async fn op_reply(tab: &mut Tab, id: CallId) -> OpOutcome {
    tab.until(|frame| match frame {
        ServerFrame::OpReply {
            id: replied,
            outcome,
        } if replied == id => Some(outcome),
        _other => None,
    })
    .await
}

fn failure(outcome: CallOutcome) -> Failure {
    match outcome {
        CallOutcome::Err(failure) => failure,
        CallOutcome::Ok(reply) => panic!("expected a failure, got {reply:?}"),
    }
}

#[tokio::test]
async fn a_call_reaches_the_node_and_failures_come_back_by_id() {
    within(Box::pin(async {
        let node = TestNode::start().await;
        let ui = Ui::start(Source::default().with("box", node.address(), node.token())).await;
        let mut tab = ui.tab().await;

        let id = tab.call("box", &Call::NodeInfo(GetNode)).await;
        match reply(&mut tab, id).await {
            CallOutcome::Ok(reply) => match *reply {
                Reply::NodeInfo(info) => assert_eq!(info.id, node.id()),
                other => panic!("{other:?}"),
            },
            CallOutcome::Err(failure) => panic!("{failure:?}"),
        }
        // The node's own error.
        let missing = GetSession {
            session: SessionId::random(),
        };
        let id = tab.call("box", &Call::GetSession(missing)).await;
        match failure(reply(&mut tab, id).await) {
            Failure::Remote { error } => assert_eq!(error.code, ErrorCode::NotFound),
            other => panic!("{other:?}"),
        }
        // No such node.
        let id = tab.call("nowhere", &Call::ListSessions(ListSessions)).await;
        match failure(reply(&mut tab, id).await) {
            Failure::Unreachable { message } => {
                assert!(message.contains("no node named nowhere"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        // Calls the bridge does not pass on, each still answered.
        let subscribe = format!(
            r#"{{"method":"session.subscribe","params":{{"session":"{}","from":0}}}}"#,
            SessionId::random()
        );
        for refused in [
            subscribe.as_str(),
            r#"{"method":"fleet.report","params":{"generation":1}}"#,
            r#"{"method":"no.such","params":null}"#,
            r#"{"method":"session.get","params":{"nope":1}}"#,
        ] {
            let id = tab.call_text("box", refused).await;
            assert!(
                matches!(failure(reply(&mut tab, id).await), Failure::Refused { .. }),
                "{refused}"
            );
        }
        // A frame that is not one is reported, and the socket stays.
        tab.send_text("{\"type\":\"call\"}").await;
        let unreadable = tab
            .until(|frame| match frame {
                ServerFrame::Unreadable { message } => Some(message),
                _other => None,
            })
            .await;
        assert!(unreadable.contains("not a frame"), "{unreadable}");
        let id = tab.call("box", &Call::ListSessions(ListSessions)).await;
        assert!(matches!(reply(&mut tab, id).await, CallOutcome::Ok(_)));
    }))
    .await;
}

#[tokio::test]
async fn every_tab_shares_one_connection_per_node() {
    within(Box::pin(async {
        let node = TestNode::start().await;
        let source = Source::default().with("box", node.address(), node.token());
        let dials = source.dials();
        let ui = Ui::start(source).await;
        let mut one = ui.tab().await;
        let mut two = ui.tab().await;
        let first = one.call("box", &Call::ListSessions(ListSessions)).await;
        let second = two.call("box", &Call::ListSessions(ListSessions)).await;
        assert!(matches!(reply(&mut one, first).await, CallOutcome::Ok(_)));
        assert!(matches!(reply(&mut two, second).await, CallOutcome::Ok(_)));
        assert_eq!(dials.load(Ordering::SeqCst), 1);

        // A tab opened later is told the node's state at once.
        let mut three = ui.tab().await;
        let status = three
            .until(|frame| match frame {
                ServerFrame::Node { node, status } if node.as_str() == "box" => Some(status),
                _other => None,
            })
            .await;
        assert!(matches!(status, NodeStatus::Connected { .. }), "{status:?}");
    }))
    .await;
}

#[tokio::test]
async fn ops_are_answered_by_the_bridge() {
    within(Box::pin(async {
        let node = TestNode::start().await;
        let ui = Ui::start(Source::default().with("box", node.address(), node.token())).await;
        let mut tab = ui.tab().await;

        let id = tab.op(Op::Nodes).await;
        match op_reply(&mut tab, id).await {
            OpOutcome::Ok(OpResult::Nodes { nodes }) => {
                let keys: Vec<_> = nodes.iter().map(|node| node.key.as_str()).collect();
                assert_eq!(keys, ["box"]);
            }
            other => panic!("{other:?}"),
        }
        let id = tab
            .op(Op::Qr {
                text: "vessel://pair#offer".to_owned(),
            })
            .await;
        match op_reply(&mut tab, id).await {
            OpOutcome::Ok(OpResult::Qr { svg }) => assert!(svg.contains("<svg"), "{svg}"),
            other => panic!("{other:?}"),
        }
        let id = tab
            .op(Op::Inspect {
                link: "not a link".to_owned(),
            })
            .await;
        assert!(matches!(
            op_reply(&mut tab, id).await,
            OpOutcome::Err { .. }
        ));
        let id = tab
            .op(Op::Retry {
                node: NodeKey::new("box"),
            })
            .await;
        assert!(matches!(
            op_reply(&mut tab, id).await,
            OpOutcome::Ok(OpResult::Retry)
        ));
    }))
    .await;
}

/// What a terminal record printed, as text.
fn printed(record: &EventRecord) -> String {
    match &record.event {
        SessionEvent::Terminal(TerminalOutput { data }) => {
            String::from_utf8_lossy(data.bytes()).into_owned()
        }
        SessionEvent::Status { .. }
        | SessionEvent::Agent { .. }
        | SessionEvent::ProfileChanged { .. } => String::new(),
    }
}

/// Records of stream `stream` until one printed `text` (all output so far
/// is searched, since a line can arrive in pieces); stream notices are
/// collected too.
async fn read_until(
    tab: &mut Tab,
    stream: StreamId,
    text: &str,
    seqs: &mut Vec<u64>,
    notices: &mut Vec<StreamState>,
) {
    let mut output = String::new();
    while !output.contains(text) {
        match tab.next().await {
            ServerFrame::Event {
                stream: from,
                record,
            } if from == stream => {
                seqs.push(record.seq.get());
                output.push_str(&printed(&record));
            }
            ServerFrame::Stream {
                stream: from,
                state,
            } if from == stream => notices.push(state),
            _other => {}
        }
    }
}

#[tokio::test]
async fn a_followed_session_resumes_after_its_node_connection_drops() {
    within(Box::pin(async {
        let node = TestNode::start().await;
        let proxy = Proxy::start(node.address()).await;
        let source = Source::default().with("box", proxy.address, node.token());
        let dials = source.dials();
        let ui = Ui::start(source).await;
        let mut tab = ui.tab().await;

        let mut spec = SessionSpec::new(
            HarnessKind::ClaudeCode,
            SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            },
        );
        spec.cwd = Some(node.dir().to_path_buf());
        let id = tab
            .call("box", &Call::CreateSession(CreateSession { spec }))
            .await;
        let session = match reply(&mut tab, id).await {
            CallOutcome::Ok(reply) => match *reply {
                Reply::CreateSession(info) => info.id,
                other => panic!("{other:?}"),
            },
            CallOutcome::Err(failure) => panic!("{failure:?}"),
        };

        let stream = StreamId::new(1);
        tab.send(&ClientFrame::Follow {
            stream,
            node: NodeKey::new("box"),
            session,
            from: EventSeq::ZERO,
        })
        .await;
        let mut seqs = Vec::new();
        let mut notices = Vec::new();
        read_until(&mut tab, stream, "ready", &mut seqs, &mut notices).await;

        // The network drops: the stream says so and comes back by itself.
        proxy.cut();
        let resumed = tab
            .until(|frame| match frame {
                ServerFrame::Stream {
                    stream: from,
                    state,
                } if from == stream => {
                    let at = match &state {
                        StreamState::Resumed { at } => Some(*at),
                        _other => None,
                    };
                    notices.push(state);
                    at
                }
                ServerFrame::Event { record, .. } => {
                    seqs.push(record.seq.get());
                    None
                }
                _other => None,
            })
            .await;
        assert!(
            matches!(notices.first(), Some(StreamState::Reconnecting { .. })),
            "{notices:?}"
        );

        // Input after the resume reaches the session, and its output the
        // stream, with nothing lost or repeated across the drop.
        let input = SendInput {
            session,
            input: SessionInput::Terminal {
                input: TerminalInput::Data {
                    data: TerminalBytes::new(Bytes::from_static(b"hello\r")),
                },
            },
        };
        let id = tab.call("box", &Call::SendInput(input)).await;
        assert!(matches!(reply(&mut tab, id).await, CallOutcome::Ok(_)));
        read_until(&mut tab, stream, "echo: hello", &mut seqs, &mut notices).await;
        let expected: Vec<u64> = (0..).take(seqs.len()).collect();
        assert_eq!(seqs, expected, "records are gap-free and unrepeated");
        assert!(resumed.get() <= u64::try_from(seqs.len()).unwrap());
        assert_eq!(dials.load(Ordering::SeqCst), 2, "one redial, shared");

        // Unfollowed: no more records for it.
        tab.send(&ClientFrame::Unfollow { stream }).await;
    }))
    .await;
}

/// Records of `stream` until `done` says the last one arrived.
async fn records_until(
    tab: &mut Tab,
    stream: StreamId,
    records: &mut Vec<EventRecord>,
    done: impl Fn(&EventRecord) -> bool,
) {
    loop {
        if let ServerFrame::Event {
            stream: from,
            record,
        } = tab.next().await
            && from == stream
        {
            let last = done(&record);
            records.push(record);
            if last {
                return;
            }
        }
    }
}

/// The answer to call `id`, keeping the records of `stream` that arrive
/// meanwhile.
async fn reply_keeping(
    tab: &mut Tab,
    id: CallId,
    stream: StreamId,
    records: &mut Vec<EventRecord>,
) -> CallOutcome {
    loop {
        match tab.next().await {
            ServerFrame::Reply {
                id: replied,
                outcome,
            } if replied == id => return outcome,
            ServerFrame::Event {
                stream: from,
                record,
            } if from == stream => records.push(record),
            _other => {}
        }
    }
}

fn agent_event(record: &EventRecord) -> Option<&AgentEvent> {
    match &record.event {
        SessionEvent::Agent { event } => Some(event),
        _other => None,
    }
}

/// A structured turn through the bridge with `fake-acp`: the records a
/// real node produces, which the page's timeline reducer is tested
/// against (`web/src/data/fixtures/fake-acp-turn.json`, written when
/// `VESSEL_WRITE_FIXTURES` is set).
#[tokio::test]
async fn a_structured_turn_with_a_permission_reaches_the_page() {
    within(Box::pin(async {
        let node = TestNode::start().await;
        let ui = Ui::start(Source::default().with("box", node.address(), node.token())).await;
        let mut tab = ui.tab().await;
        let mut spec = SessionSpec::new(
            HarnessKind::ClaudeCode,
            SessionMode::Structured {
                model: None,
                resume: None,
            },
        );
        spec.cwd = Some(node.dir().to_path_buf());
        let id = tab
            .call("box", &Call::CreateSession(CreateSession { spec }))
            .await;
        let session = match reply(&mut tab, id).await {
            CallOutcome::Ok(reply) => match *reply {
                Reply::CreateSession(info) => info.id,
                other => panic!("{other:?}"),
            },
            CallOutcome::Err(failure) => panic!("{failure:?}"),
        };
        let stream = StreamId::new(1);
        tab.send(&ClientFrame::Follow {
            stream,
            node: NodeKey::new("box"),
            session,
            from: EventSeq::ZERO,
        })
        .await;
        let mut records = Vec::new();
        records_until(&mut tab, stream, &mut records, |record| {
            matches!(agent_event(record), Some(AgentEvent::Ready { .. }))
        })
        .await;
        let prompt = SendInput {
            session,
            input: SessionInput::Prompt {
                content: vec![ContentBlock::Text {
                    text: "tidy the overview".to_owned(),
                }],
            },
        };
        let id = tab.call("box", &Call::SendInput(prompt)).await;
        let sent = reply_keeping(&mut tab, id, stream, &mut records).await;
        assert!(matches!(sent, CallOutcome::Ok(_)));
        let asked = |record: &EventRecord| {
            matches!(
                agent_event(record),
                Some(AgentEvent::PermissionRequested(_))
            )
        };
        if !records.iter().any(asked) {
            records_until(&mut tab, stream, &mut records, asked).await;
        }
        let Some(AgentEvent::PermissionRequested(request)) = records
            .iter()
            .filter_map(agent_event)
            .find(|event| matches!(event, AgentEvent::PermissionRequested(_)))
            .cloned()
        else {
            panic!("no permission request");
        };
        let answer = SendInput {
            session,
            input: SessionInput::AnswerPermission {
                id: request.id,
                outcome: PermissionOutcome::Selected {
                    option: PermissionOptionId::new("allow-once"),
                },
            },
        };
        let id = tab.call("box", &Call::SendInput(answer)).await;
        let answered = reply_keeping(&mut tab, id, stream, &mut records).await;
        assert!(matches!(answered, CallOutcome::Ok(_)));
        let ended = |record: &EventRecord| {
            matches!(agent_event(record), Some(AgentEvent::TurnEnded { .. }))
        };
        if !records.iter().any(ended) {
            records_until(&mut tab, stream, &mut records, ended).await;
        }
        let seqs: Vec<u64> = records.iter().map(|record| record.seq.get()).collect();
        let expected: Vec<u64> = (0..).take(seqs.len()).collect();
        assert_eq!(seqs, expected);
        if std::env::var_os("VESSEL_WRITE_FIXTURES").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../web/src/data/fixtures/fake-acp-turn.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut text = serde_json::to_string_pretty(&records).unwrap();
            text.push('\n');
            std::fs::write(path, text).unwrap();
        }
    }))
    .await;
}

#[tokio::test]
async fn unknown_names_cannot_fill_the_registry_and_removed_nodes_are_pruned() {
    within(Box::pin(async {
        let node = TestNode::start().await;
        let source = Source::default().with("box", node.address(), node.token());
        let control = source.clone();
        let ui = Ui::start(source).await;
        let mut tab = ui.tab().await;
        for n in 0..260 {
            let id = tab
                .op(Op::Retry {
                    node: NodeKey::new(format!("missing-{n}")),
                })
                .await;
            let failed = tab
                .until(|frame| match frame {
                    ServerFrame::OpReply { id: got, outcome } if id == got => Some(outcome),
                    _ => None,
                })
                .await;
            assert!(matches!(failed, OpOutcome::Err { .. }));
        }
        let id = tab.call("box", &Call::ListSessions(ListSessions)).await;
        assert!(matches!(reply(&mut tab, id).await, CallOutcome::Ok(_)));
        control.remove_all();
        let id = tab.op(Op::Nodes).await;
        let mut removed = false;
        let mut listed = false;
        while !removed || !listed {
            match tab.next().await {
                ServerFrame::Node {
                    node,
                    status: NodeStatus::Failed { .. },
                } if node.as_str() == "box" => removed = true,
                ServerFrame::OpReply {
                    id: got,
                    outcome: OpOutcome::Ok(OpResult::Nodes { nodes }),
                } if id == got => {
                    assert!(nodes.is_empty());
                    listed = true;
                }
                _ => {}
            }
        }
        let id = tab.call("box", &Call::ListSessions(ListSessions)).await;
        assert!(matches!(reply(&mut tab, id).await, CallOutcome::Err(_)));
        // A new tab receives no retained entry in its initial state push.
        let mut other = ui.tab().await;
        let id = other.op(Op::Nodes).await;
        loop {
            match other.next().await {
                ServerFrame::Node { .. } => panic!("removed node retained"),
                ServerFrame::OpReply { id: got, .. } if id == got => break,
                _ => {}
            }
        }
    }))
    .await;
}
