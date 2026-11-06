//! Agent principals end to end (milestone 4 Track H4, ADR 0013): every
//! session's agent gets the `vessel` MCP server with its own credential;
//! with it, the agent reaches the node's local socket and may list, start,
//! drive and stop the sessions it started, within its creator's grant and
//! the limits. `fake-acp` reports the servers it was given, which is how
//! these tests play the agent's `vessel mcp`.

use std::path::PathBuf;

use vessel_client::{LocalConnectError, LocalCredential, NodeClient, SessionStream};
use vessel_node::VesselMcp;
use vessel_proto::{
    AgentAuth, AgentSecret, Creator, ErrorCode, EventRecord, EventSeq, HarnessKind, ImportSession,
    SessionId, SessionRef, SessionStatus, StopWhen,
};
use vessel_rpc::{CallError, ConnectError};

use crate::support::{
    Fixture, ask, client, keys, read_until, status_of, structured_spec, terminal_spec, within,
};

/// What `vessel mcp` would be run as; never run here.
pub(crate) fn vessel_mcp(fixture: &Fixture) -> VesselMcp {
    VesselMcp::vessel(fixture.cwd().join("vessel"))
}

/// A running structured session and its event stream.
pub(crate) struct Running {
    pub(crate) id: SessionId,
    stream: SessionStream,
    seen: Vec<EventRecord>,
}

pub(crate) async fn structured(creator: &NodeClient, fixture: &Fixture) -> Running {
    let id = creator
        .create_session(structured_spec(fixture, None))
        .await
        .unwrap()
        .id;
    let mut stream = creator.follow(id, EventSeq::ZERO).await.unwrap();
    let mut seen = Vec::new();
    read_until(&mut stream, &mut seen, |record| {
        status_of(record) == Some(&SessionStatus::Running)
    })
    .await;
    Running { id, stream, seen }
}

/// The session's `vessel` server as its agent got it: the program, the
/// node's endpoint, and the credential.
pub(crate) async fn credential(
    owner: &NodeClient,
    session: &mut Running,
) -> (PathBuf, PathBuf, AgentAuth) {
    let mcp = ask(
        owner,
        &mut session.stream,
        &mut session.seen,
        session.id,
        "mcp",
    )
    .await;
    let servers: serde_json::Value =
        serde_json::from_str(mcp.strip_prefix("mcp ").unwrap()).unwrap();
    let vessel = servers
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == "vessel")
        .unwrap_or_else(|| panic!("no vessel server: {servers}"));
    assert_eq!(vessel["args"], serde_json::json!(["mcp"]));
    let variable = |name: &str| {
        vessel["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variable| variable["name"] == name)
            .and_then(|variable| variable["value"].as_str())
            .unwrap_or_else(|| panic!("{name} missing: {vessel}"))
            .to_owned()
    };
    let auth = AgentAuth {
        session: variable("VESSEL_AGENT_SESSION").parse().unwrap(),
        secret: variable("VESSEL_AGENT_SECRET").parse().unwrap(),
    };
    assert_eq!(auth.session, session.id);
    (
        PathBuf::from(vessel["command"].as_str().unwrap()),
        PathBuf::from(variable("VESSEL_NODE_SOCKET")),
        auth,
    )
}

pub(crate) async fn as_agent(socket: &std::path::Path, auth: &AgentAuth) -> NodeClient {
    NodeClient::connect_local(socket, &LocalCredential::Agent(auth.clone()), client())
        .await
        .unwrap()
}

pub(crate) fn code<T: std::fmt::Debug>(result: Result<T, CallError>) -> ErrorCode {
    match result {
        Err(CallError::Remote(error)) => error.code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn refused(result: Result<NodeClient, LocalConnectError>) -> bool {
    matches!(
        result,
        Err(LocalConnectError::Handshake(ConnectError::Rejected(error)))
            if error.code == ErrorCode::Unauthorized
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_drives_the_sessions_it_started_and_no_others() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture
            .start_with(|config| config.vessel_mcp = Some(vessel_mcp(&fixture)))
            .await;
        let owner = fixture.connect(&node).await;
        let node_id = owner.node().id;

        let mut first = structured(&owner, &fixture).await;
        let (program, socket, auth) = credential(&owner, &mut first).await;
        assert_eq!(program, vessel_mcp(&fixture).program);
        let agent = as_agent(&socket, &auth).await;

        // It sees the node's sessions and starts its own.
        let listed = agent.sessions().await.unwrap();
        assert!(listed.iter().any(|info| info.id == first.id));
        let started = agent
            .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
            .await
            .unwrap();
        assert_eq!(
            started.created_by,
            Creator::Agent {
                node: node_id,
                session: first.id,
            }
        );
        agent.send(started.id, keys("hello\r")).await.unwrap();

        // Not the owner's sessions, nor another agent's; never `admin`.
        let theirs = structured(&owner, &fixture).await;
        assert_eq!(code(agent.kill(theirs.id).await), ErrorCode::Forbidden);
        assert_eq!(code(agent.kill(first.id).await), ErrorCode::Forbidden);
        // Nor copy them away (protocol 6). The import names a profile this
        // node lacks, so a broken check fails without reading any home.
        assert_eq!(
            code(agent.export_session(theirs.id, true).await),
            ErrorCode::Forbidden
        );
        let copy = ImportSession {
            from: SessionRef {
                node: node_id,
                session: theirs.id,
            },
            profile: Some("nope".parse().unwrap()),
            cwd: None,
            start: false,
        };
        assert_eq!(code(agent.import_session(copy).await), ErrorCode::Forbidden);
        let mut other = theirs;
        let (_, _, other_auth) = credential(&owner, &mut other).await;
        let other_agent = as_agent(&socket, &other_auth).await;
        assert_eq!(
            code(other_agent.kill(started.id).await),
            ErrorCode::Forbidden
        );
        assert_eq!(
            code(agent.stop_node(StopWhen::Now).await),
            ErrorCode::Forbidden
        );
        agent.kill(started.id).await.unwrap();

        // A forged secret, or a real one for another session, is refused.
        let forged = AgentAuth {
            session: auth.session,
            secret: AgentSecret::from_bytes([0; 32]),
        };
        assert!(refused(
            NodeClient::connect_local(&socket, &LocalCredential::Agent(forged), client()).await
        ));
        let borrowed = AgentAuth {
            session: other.id,
            secret: auth.secret.clone(),
        };
        assert!(refused(
            NodeClient::connect_local(&socket, &LocalCredential::Agent(borrowed), client()).await
        ));

        // The credential dies with its session, and the connection with it.
        owner.kill(first.id).await.unwrap();
        agent.closed().await;
        assert!(refused(
            NodeClient::connect_local(&socket, &LocalCredential::Agent(auth), client()).await
        ));
        node.stop().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_are_held_to_the_count_and_depth_limits() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture
            .start_with(|config| config.vessel_mcp = Some(vessel_mcp(&fixture)))
            .await;
        let owner = fixture.connect(&node).await;

        // Depth: user session (0) -> 1 -> 2 -> 3; the agent at 3 may not.
        let mut session = structured(&owner, &fixture).await;
        let (_, socket, auth) = credential(&owner, &mut session).await;
        let top = as_agent(&socket, &auth).await;
        let mut agent = as_agent(&socket, &auth).await;
        let mut deepest = session.id;
        for _level in 1..=3 {
            let mut child = structured(&agent, &fixture).await;
            let (_, _, auth) = credential(&agent, &mut child).await;
            deepest = child.id;
            agent = as_agent(&socket, &auth).await;
        }
        assert_eq!(
            code(
                agent
                    .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
                    .await
            ),
            ErrorCode::Forbidden
        );
        // The top agent owns its descendants: it may stop the deepest.
        top.kill(deepest).await.unwrap();

        // Count: eight live sessions of its own, not nine; one ending frees
        // a place.
        let mut session = structured(&owner, &fixture).await;
        let (_, _, auth) = credential(&owner, &mut session).await;
        let busy = as_agent(&socket, &auth).await;
        let mut children = Vec::new();
        for _ in 0..8 {
            children.push(
                busy.create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
                    .await
                    .unwrap()
                    .id,
            );
        }
        assert_eq!(
            code(
                busy.create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
                    .await
            ),
            ErrorCode::Forbidden
        );
        let ended = children.pop().unwrap();
        busy.kill(ended).await.unwrap();
        let mut stream = owner.follow(ended, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        read_until(&mut stream, &mut seen, |record| {
            status_of(record).is_some_and(|status| !status.is_live())
        })
        .await;
        let _freed = busy
            .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
            .await
            .unwrap();
        node.stop().await.unwrap();
    })
    .await;
}

/// Once a session's end is visible, its agent is done here, even before
/// the node retires it (which waits for the actor to finish): a new
/// connection is refused, and one still open can start nothing. Without
/// the `has_ended` checks this failed only when the test won the race
/// against `retire`; with them it holds every time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_sessions_agent_starts_nothing() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture
            .start_with(|config| config.vessel_mcp = Some(vessel_mcp(&fixture)))
            .await;
        let owner = fixture.connect(&node).await;
        let mut session = structured(&owner, &fixture).await;
        let (_program, socket, auth) = credential(&owner, &mut session).await;
        let agent = as_agent(&socket, &auth).await;

        owner.kill(session.id).await.unwrap();
        read_until(&mut session.stream, &mut session.seen, |record| {
            status_of(record).is_some_and(|status| !status.is_live())
        })
        .await;

        assert!(
            agent
                .create_session(terminal_spec(&fixture, HarnessKind::ClaudeCode))
                .await
                .is_err()
        );
        assert!(refused(
            NodeClient::connect_local(&socket, &LocalCredential::Agent(auth), client()).await
        ));
        node.stop().await.unwrap();
    })
    .await;
}
