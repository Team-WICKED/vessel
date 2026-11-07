//! `vessel mcp`, the binary itself (ADR 0013): the node attaches it to a
//! session as the `vessel` MCP server; run with the environment the node
//! gave that session's agent, it serves the tools on stdio, and without
//! that environment it says what is missing.

use std::path::PathBuf;
use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;
use vessel_client::NodeClient;
use vessel_node::{DataDir, VesselMcp};
use vessel_proto::{
    AgentEvent, ClientInfo, ContentBlock, EventSeq, HarnessKind, SessionEvent, SessionInput,
    SessionMode, SessionSpec, SessionStatus,
};

use crate::support::{Fixture, within};

const VESSEL: &str = env!("CARGO_BIN_EXE_vessel");

/// The `vessel` server's environment as a new structured session's agent
/// got it (`fake-acp` reports its servers with their values).
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "a test reading one session's log for the few records it needs"
)]
async fn vessel_server_env(fixture: &Fixture) -> Vec<(String, String)> {
    let token = DataDir::new(fixture.data())
        .read_token()
        .unwrap()
        .expose()
        .parse()
        .unwrap();
    let url = fixture.node_address().parse().unwrap();
    let client = ClientInfo {
        name: "vessel-cli-test".to_owned(),
        version: "0".to_owned(),
    };
    let owner = NodeClient::connect(&url, &token, client).await.unwrap();
    let spec = SessionSpec::new(
        HarnessKind::ClaudeCode,
        SessionMode::Structured {
            model: None,
            resume: None,
        },
    )
    .with_cwd(PathBuf::from(fixture.cwd()));
    let id = owner.create_session(spec).await.unwrap().id;
    let mut stream = owner.follow(id, EventSeq::ZERO).await.unwrap();
    let mut reply = String::new();
    while let Some(record) = stream.next().await {
        match record.unwrap().event {
            SessionEvent::Status {
                status: SessionStatus::Running,
            } => {
                let prompt = SessionInput::Prompt {
                    content: vec![ContentBlock::Text {
                        text: "mcp".to_owned(),
                    }],
                };
                owner.send(id, prompt).await.unwrap();
            }
            SessionEvent::Agent {
                event:
                    AgentEvent::MessageChunk {
                        content: ContentBlock::Text { text },
                    },
            } => reply.push_str(&text),
            SessionEvent::Agent {
                event: AgentEvent::TurnEnded { .. },
            } => break,
            _ => {}
        }
    }
    let servers: Value = serde_json::from_str(reply.strip_prefix("mcp ").unwrap()).unwrap();
    let vessel = servers
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == "vessel")
        .unwrap();
    assert_eq!(vessel["command"], VESSEL);
    vessel["env"]
        .as_array()
        .unwrap()
        .iter()
        .map(|variable| {
            (
                variable["name"].as_str().unwrap().to_owned(),
                variable["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vessel_mcp_serves_the_tools_on_stdio() {
    within(async {
        let fixture = Fixture::start_with(|config, _| {
            config.vessel_mcp = Some(VesselMcp::vessel(PathBuf::from(VESSEL)));
        })
        .await;
        let env = vessel_server_env(&fixture).await;

        let mut command = Command::new(VESSEL);
        let _command = command
            .arg("mcp")
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        for message in [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            } }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ] {
            stdin
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        }
        let mut tools = Vec::new();
        while let Some(line) = stdout.next_line().await.unwrap() {
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == json!(2) {
                tools = message["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                    .collect();
                break;
            }
        }
        assert!(
            tools.iter().any(|tool| tool == "create_session"),
            "{tools:?}"
        );
        drop(stdin);
        let status = child.wait().await.unwrap();
        assert!(status.success(), "{status}");
    })
    .await;
}

#[tokio::test]
async fn vessel_mcp_without_its_environment_says_what_is_missing() {
    let output = Command::new(VESSEL)
        .arg("mcp")
        .env_remove("VESSEL_NODE_SOCKET")
        .env_remove("VESSEL_AGENT_SESSION")
        .env_remove("VESSEL_AGENT_SECRET")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("VESSEL_NODE_SOCKET is not set"), "{stderr}");
}
