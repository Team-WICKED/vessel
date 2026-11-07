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
    Call, CreateSession, ErrorCode, EventRecord, EventSeq, GetNode, GetSession, HarnessKind,
    ListSessions, Reply, SendInput, SessionEvent, SessionId, SessionInput, SessionMode,
    SessionSpec, TerminalBytes, TerminalInput, TerminalOutput, TerminalSize,
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
