//! The node's local socket (ADR 0007) and `node.stop` with draining
//! (protocol 3), against an in-process node with no TCP listener.

use std::path::Path;

use futures_util::StreamExt as _;
use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};
use vessel_client::NodeClient;
use vessel_proto::{
    Ack, AgentEvent, ErrorCode, EventRecord, EventSeq, GetNode, NodeState, ServerFrame,
    SessionStatus, StopNode, StopWhen,
};
use vessel_rpc::{ClientConfig, Connection, Framed, MAX_FRAME};

use crate::support::{
    Fixture, agent_of, client, prompt, read_until, status_of, structured_spec, within,
};

#[cfg(unix)]
type LocalStream = tokio::net::UnixStream;
#[cfg(windows)]
type LocalStream = tokio::net::windows::named_pipe::NamedPipeClient;

type LocalFramed = Framed<ReadHalf<LocalStream>, WriteHalf<LocalStream>>;

#[cfg(unix)]
async fn open(endpoint: &Path) -> LocalStream {
    tokio::net::UnixStream::connect(endpoint).await.unwrap()
}

/// Retries `ERROR_PIPE_BUSY` (231), as the node's own `attach` does: every
/// pipe instance is briefly busy while the node creates the next one.
#[cfg(windows)]
async fn open(endpoint: &Path) -> LocalStream {
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint) {
            Err(error) if error.raw_os_error() == Some(231) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            opened => return opened.unwrap(),
        }
    }
}

/// A connection to the node's local endpoint that presented `token`.
async fn framed(endpoint: &Path, token: &str) -> LocalFramed {
    let mut stream = open(endpoint).await;
    let length = u32::try_from(token.len()).unwrap().to_be_bytes();
    stream.write_all(&length).await.unwrap();
    stream.write_all(token.as_bytes()).await.unwrap();
    let (reader, writer) = tokio::io::split(stream);
    Framed::new(reader, writer, MAX_FRAME)
}

fn token(fixture: &Fixture) -> String {
    fixture.data_dir().read_token().unwrap().expose().to_owned()
}

async fn control(endpoint: &Path, token: &str) -> Connection {
    Connection::connect(
        framed(endpoint, token).await,
        client(),
        &ClientConfig::default(),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_local_socket_serves_the_token_holder_and_node_stop_ends_the_node() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start_with(|config| config.bind = None).await;
        assert_eq!(node.address(), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(node.local_endpoint())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let token = token(&fixture);
        let connection = control(node.local_endpoint(), &token).await;
        assert_eq!(connection.welcome().node.id, node.info().id);
        assert_eq!(
            connection.call(GetNode).await.unwrap().state,
            NodeState::Serving
        );

        // A wrong token is answered with `rejected` (unauthorized) and the
        // connection closes before anything is served.
        let mut refused = framed(node.local_endpoint(), "not-the-token").await;
        let answer = refused.next().await.unwrap().unwrap();
        let Ok(ServerFrame::Rejected { error }) = serde_json::from_str(&answer) else {
            panic!("expected a refusal, got {answer}");
        };
        assert_eq!(error.code, ErrorCode::Unauthorized);
        // Nothing follows: the end of the stream, or (Windows pipes) a
        // broken-pipe error.
        assert!(!matches!(refused.next().await, Some(Ok(_))));

        // `node.stop{now}` is answered, then the node stops on its own.
        let stopped = connection
            .call(StopNode {
                when: StopWhen::Now,
            })
            .await;
        assert_eq!(stopped.unwrap(), Ack);
        node.stop_requested().await;
        node.stop().await.unwrap();
    })
    .await;
}

fn running(record: &EventRecord) -> bool {
    status_of(record) == Some(&SessionStatus::Running)
}

fn turn_ended(record: &EventRecord) -> bool {
    matches!(agent_of(record), Some(AgentEvent::TurnEnded { .. }))
}

/// Draining (ADR 0008 `--when-idle`): new sessions are refused, the live
/// one keeps working, new connections are told, and the node stops once
/// the last live session has ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draining_refuses_new_sessions_and_stops_once_idle() {
    within(async {
        let fixture = Fixture::new();
        let node = fixture.start_with(|config| config.bind = None).await;
        let token = token(&fixture);
        let endpoint = node.local_endpoint().to_path_buf();
        let client = NodeClient::over(
            framed(&endpoint, &token).await,
            client(),
            &ClientConfig::default(),
        )
        .await
        .unwrap();
        let live = client
            .create_session(structured_spec(&fixture, None))
            .await
            .unwrap()
            .id;
        let mut view = client.follow(live, EventSeq::ZERO).await.unwrap();
        let mut seen = Vec::new();
        read_until(&mut view, &mut seen, running).await;

        let control = control(&endpoint, &token).await;
        let drained = control.call(StopNode {
            when: StopWhen::WhenIdle,
        });
        assert_eq!(drained.await.unwrap(), Ack);
        assert_eq!(node.info().state, NodeState::Draining);
        let late = control_welcome_state(&endpoint, &token).await;
        assert_eq!(late, NodeState::Draining);
        let refused = client
            .create_session(structured_spec(&fixture, None))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Some(ErrorCode::Unavailable));

        client.send(live, prompt("plain")).await.unwrap();
        read_until(&mut view, &mut seen, turn_ended).await;

        client.kill(live).await.unwrap();
        node.stop_requested().await;
        node.stop().await.unwrap();
    })
    .await;
}

/// The node state a new connection's `Welcome` reports.
async fn control_welcome_state(endpoint: &Path, token: &str) -> NodeState {
    control(endpoint, token).await.welcome().node.state
}
