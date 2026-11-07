//! Calls across a fleet (protocol 6, Track X): a home and two members, all
//! linked to an in-process relay. Each member grants the home a credential
//! on itself when it connects; calls go caller → its node → home → target
//! in `fleet.forward`, `fleet.relay` and `fleet.deliver`, and the target
//! runs them as the delivering device cut down to the origin, with
//! ownership and the agent limits applied to the origin.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream};
use vessel_client::NodeClient;
use vessel_node::Node;
use vessel_node::fleet::{self, DEFAULT_GRANT, FleetFiles, FleetInit};
use vessel_proto::{
    CreateSession, Creator, ErrorCode, EventSeq, GetSession, HarnessKind, KillSession, ListNodes,
    ListPeers, ListSessions, NodeId, ReadEvents, RemoteCreator, Scope, SendInput, SessionId,
    SessionOrigin, SessionStatus,
};

use crate::agents::{as_agent, code, credential, structured, vessel_mcp};
use crate::fleet::{fleet_config, write};
use crate::secure::{Relay, relayed_with};
use crate::support::{Fixture, prompt, read_until, status_of, structured_spec, within};

/// One node of the test fleet.
pub(crate) struct Member {
    pub(crate) fixture: Fixture,
    pub(crate) node: Node,
    pub(crate) owner: NodeClient,
}

impl Member {
    pub(crate) fn id(&self) -> NodeId {
        self.node.info().id
    }
}

/// A home with `fleet.toml`, linked to `relay`.
pub(crate) async fn home(relay: &Relay, sources: &std::path::Path) -> Member {
    let agents = sources.join("AGENTS.md");
    write(&agents, "fleet instructions\n");
    std::fs::create_dir_all(sources.join("skills")).unwrap();
    let fixture = Fixture::new();
    let _toml = fleet::init(
        &FleetFiles::new(&fixture.data_dir()),
        &FleetInit {
            instructions: Some(agents),
            skills: Some(sources.join("skills")),
            skip: Vec::new(),
        },
    )
    .unwrap();
    let applied = fixture.cwd().join("home-claude");
    let node = relayed_with(&fixture, &[&relay.url], |config| {
        config.name = Some("home".to_owned());
        config.fleet = fleet_config(HarnessKind::ClaudeCode, applied);
        config.vessel_mcp = Some(vessel_mcp(&fixture));
    })
    .await;
    let owner = fixture.connect(&node).await;
    Member {
        fixture,
        node,
        owner,
    }
}

/// A member named `label` that joined `home` with a link of `scopes`,
/// granting the home the default (`read`, `operate`); returned once the
/// home can call it.
async fn member(relay: &Relay, home: &Member, label: &str, scopes: Vec<Scope>) -> Member {
    member_granting(relay, home, label, scopes, DEFAULT_GRANT.to_vec()).await
}

/// A member as [`member`] makes one, granting the home `grant`; returned
/// once the home can call it.
pub(crate) async fn member_granting(
    relay: &Relay,
    home: &Member,
    label: &str,
    scopes: Vec<Scope>,
    grant: Vec<Scope>,
) -> Member {
    let fixture = Fixture::new();
    let applied = fixture.cwd().join("member-codex");
    let node = relayed_with(&fixture, &[&relay.url], |config| {
        config.name = Some(label.to_owned());
        config.fleet = fleet_config(HarnessKind::Codex, applied);
        config.vessel_mcp = Some(vessel_mcp(&fixture));
    })
    .await;
    let offer = home
        .owner
        .invite_with_scopes(Some(label.to_owned()), 600, scopes)
        .await
        .unwrap()
        .link
        .decode()
        .unwrap();
    let _joined = fleet::join(&FleetFiles::new(&fixture.data_dir()), &offer, label, grant)
        .await
        .unwrap();
    let id = node.info().id;
    // Usable once both sides recorded the grant: the home lists it, and the
    // member wrote down which device is its home's (it takes deliveries
    // only from that one). The member records after the home does, so
    // the home's list alone was not enough (CI saw a delivered call
    // refused in between).
    let granted = FleetFiles::new(&fixture.data_dir())
        .dir()
        .join("granted.json");
    for _ in 0..800 {
        let peers = home.owner.call(ListPeers).await.unwrap();
        if peers.iter().any(|peer| peer.node == id) && granted.is_file() {
            let owner = fixture.connect(&node).await;
            return Member {
                fixture,
                node,
                owner,
            };
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{label} never granted the home access");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_reach_members_through_the_home_within_every_grant() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let relaying = member(&relay, &home, "laptop", vec![Scope::Sync, Scope::Relay]).await;
        let plain = member(&relay, &home, "desktop", vec![Scope::Sync]).await;

        // Every node sees the fleet: the home answers, members ask it.
        for node in [&home, &relaying, &plain] {
            let nodes = node.owner.call(ListNodes).await.unwrap();
            assert_eq!(nodes.len(), 3, "{nodes:?}");
        }

        // The home's owner starts a session on a member: the member records
        // the home's device as its creator, and where it came from.
        let created = home
            .owner
            .call_on(
                relaying.id(),
                CreateSession {
                    spec: structured_spec(&relaying.fixture, None),
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(created.created_by, Creator::Device { .. }),
            "{:?}",
            created.created_by
        );
        assert_eq!(created.via, Some(home.id()));
        assert_eq!(
            created.origin,
            Some(SessionOrigin::Remote {
                node: home.id(),
                by: RemoteCreator::Owner,
            })
        );
        let local = relaying.owner.session(created.id).await.unwrap();
        assert_eq!(local.origin, created.origin);

        // A member with `relay` reaches the other member through the home.
        let listed = relaying
            .owner
            .call_on(plain.id(), ListSessions)
            .await
            .unwrap();
        assert!(listed.is_empty());
        let theirs = relaying
            .owner
            .call_on(
                plain.id(),
                CreateSession {
                    spec: structured_spec(&plain.fixture, None),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            theirs.origin,
            Some(SessionOrigin::Remote {
                node: relaying.id(),
                by: RemoteCreator::Owner,
            })
        );

        // Without `relay`, the home refuses to pass a member's call on; and
        // the home itself grants its members nothing but `sync` and `relay`.
        let refused = plain.owner.call_on(relaying.id(), ListSessions).await;
        assert_eq!(code(refused), ErrorCode::Forbidden);
        let on_home = relaying.owner.call_on(home.id(), ListSessions).await;
        assert_eq!(code(on_home), ErrorCode::Forbidden);

        // A long poll across two hops returns once a record lands.
        long_poll_across_two_hops(&relaying, &plain, theirs.id).await;
        relay.stop().await;
    })
    .await;
}

/// `caller` waits on `session` of `target` through the home, and the read
/// returns once a prompt sent the same way makes records there: woken by
/// them, well before its wait runs out. The session runs before the wait
/// starts, so no change of status can be what wakes it.
async fn long_poll_across_two_hops(caller: &Member, target: &Member, session: SessionId) {
    let mut stream = target.owner.follow(session, EventSeq::ZERO).await.unwrap();
    read_until(&mut stream, &mut Vec::new(), |record| {
        status_of(record) == Some(&SessionStatus::Running)
    })
    .await;
    let asked = std::time::Instant::now();
    let from = target
        .owner
        .call(GetSession { session })
        .await
        .unwrap()
        .next_seq;
    let waiting = {
        let client = caller.owner.clone();
        let node = target.id();
        tokio::spawn(async move {
            client
                .call_on(
                    node,
                    ReadEvents {
                        session,
                        from,
                        limit: 50,
                        wait: Some(30),
                    },
                )
                .await
        })
    };
    // A head start for the read, so it is usually waiting when the prompt
    // lands; if it is not, it finds the records at once, which the
    // assertions accept too. Nothing here depends on the timing.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _sent = caller
        .owner
        .call_on(
            target.id(),
            SendInput {
                session,
                input: prompt("hello"),
            },
        )
        .await
        .unwrap();
    let page = waiting.await.unwrap().unwrap();
    assert!(
        asked.elapsed() < Duration::from_secs(15),
        "the read slept through the records"
    );
    assert!(!page.records.is_empty());
    assert!(page.records.iter().all(|record| record.seq >= from));
    let _killed = caller
        .owner
        .call_on(target.id(), KillSession { session })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_on_another_node_drives_only_what_it_started() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let laptop = member(&relay, &home, "laptop", vec![Scope::Sync, Scope::Relay]).await;
        let desktop = member(&relay, &home, "desktop", vec![Scope::Sync]).await;

        // An agent of a session on the laptop.
        let mut session = structured(&laptop.owner, &laptop.fixture).await;
        let (_program, socket, auth) = credential(&laptop.owner, &mut session).await;
        let agent = as_agent(&socket, &auth).await;

        // It starts a session on the desktop, recorded as its own there.
        let started = agent
            .call_on(
                desktop.id(),
                CreateSession {
                    spec: structured_spec(&desktop.fixture, None),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            started.created_by,
            Creator::Agent {
                node: laptop.id(),
                session: session.id,
            }
        );
        assert_eq!(started.origin, None, "an agent's node is in created_by");
        assert_eq!(started.via, Some(home.id()));

        // Not the desktop owner's session; its own, yes.
        let owners = desktop
            .owner
            .create_session(structured_spec(&desktop.fixture, None))
            .await
            .unwrap();
        let refused = agent
            .call_on(desktop.id(), KillSession { session: owners.id })
            .await;
        assert_eq!(code(refused), ErrorCode::Forbidden);
        let _killed = agent
            .call_on(
                desktop.id(),
                KillSession {
                    session: started.id,
                },
            )
            .await
            .unwrap();

        // Never `admin`, wherever the call goes.
        let revoke = agent.call_on(desktop.id(), vessel_proto::ListDevices).await;
        assert_eq!(code(revoke), ErrorCode::InvalidRequest, "not forwardable");
        relay.stop().await;
    })
    .await;
}

/// An agent's `vessel` tools served on a pipe, the test speaking MCP's
/// JSON-RPC lines to them as a harness would.
struct Tools {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
    write: tokio::io::WriteHalf<DuplexStream>,
    next: u64,
}

impl Tools {
    /// Serve the tools of `agent` (connected with its session's
    /// credential) and initialise them.
    async fn serve(agent: NodeClient) -> Self {
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(server);
        drop(tokio::spawn(::vessel_mcp::serve(
            ::vessel_mcp::VesselTools::new(agent),
            read,
            write,
        )));
        let (read, write) = tokio::io::split(client);
        let mut tools = Self {
            lines: BufReader::new(read).lines(),
            write,
            next: 1,
        };
        let initialized = tools
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                }),
            )
            .await;
        assert_eq!(initialized["result"]["serverInfo"]["name"], "vessel");
        tools
            .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        tools
    }

    async fn send(&mut self, message: &Value) {
        self.write
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    /// A request's response (`result` or `error`).
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next = self.next.saturating_add(1);
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        loop {
            let line = self.lines.next_line().await.unwrap().expect("server left");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    /// A tool's result as JSON, or the text of its failure.
    async fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, String> {
        let response = self
            .request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            )
            .await;
        let result = &response["result"];
        assert!(result.is_object(), "{response}");
        let text = result["content"][0]["text"].as_str().unwrap();
        if result["isError"] == json!(true) {
            Err(text.to_owned())
        } else {
            Ok(serde_json::from_str(text).unwrap())
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agents_tools_reach_another_node_by_its_name() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let laptop = member(&relay, &home, "laptop", vec![Scope::Sync, Scope::Relay]).await;
        let desktop = member(&relay, &home, "desktop", vec![Scope::Sync]).await;
        let mut session = structured(&laptop.owner, &laptop.fixture).await;
        let (_program, socket, auth) = credential(&laptop.owner, &mut session).await;
        let mut tools = Tools::serve(as_agent(&socket, &auth).await).await;

        // The fleet, as the home lists it.
        let nodes = tools.call("list_nodes", json!({})).await.unwrap();
        let mut names: Vec<&str> = nodes
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["desktop", "home", "laptop"], "{nodes}");

        // A conversation on the desktop, named: started, prompted, and its
        // reply waited for in long polls across two hops.
        let created = tools
            .call(
                "create_session",
                json!({
                    "node": "desktop",
                    "harness": "claude",
                    "cwd": desktop.fixture.cwd(),
                    "prompt": "plain"
                }),
            )
            .await
            .unwrap();
        let started: SessionId = serde_json::from_value(created["session"]["id"].clone()).unwrap();
        assert_eq!(created["session"]["via"], json!(home.id()));
        let turn = tools
            .call(
                "wait_turn",
                json!({
                    "node": "desktop",
                    "session": started,
                    "from": created["from"],
                    "timeout_secs": 30
                }),
            )
            .await
            .unwrap();
        assert_eq!(turn["outcome"], "ended", "{turn}");
        assert_eq!(turn["reply"], "echo: plain");
        let there = desktop.owner.session(started).await.unwrap();
        assert_eq!(
            there.created_by,
            Creator::Agent {
                node: laptop.id(),
                session: session.id,
            }
        );

        // Its node by id too; left out, the call stays on the laptop.
        let page = tools
            .call(
                "read_events",
                json!({ "node": desktop.id(), "session": started }),
            )
            .await
            .unwrap();
        assert!(!page["records"].as_array().unwrap().is_empty(), "{page}");
        let here = tools
            .call("read_events", json!({ "session": started }))
            .await;
        assert!(here.unwrap_err().starts_with("\"not_found\""));

        // Stopped the same way. A node the fleet does not have is an error
        // of the call.
        let _killed = tools
            .call(
                "kill_session",
                json!({ "node": "desktop", "session": started }),
            )
            .await
            .unwrap();
        let unknown = tools
            .request(
                "tools/call",
                json!({ "name": "list_sessions", "arguments": { "node": "elsewhere" } }),
            )
            .await;
        assert!(unknown["error"].is_object(), "{unknown}");
        relay.stop().await;
    })
    .await;
}
