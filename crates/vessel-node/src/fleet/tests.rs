//! Fleet sync end to end inside the process: a home node publishing its
//! sources, and a member's pull loop reaching it over in-memory channels
//! that stand in for the secure channel (the node's device gateway, the
//! `sync` scope check and the fleet calls are the real ones), applying
//! into temporary harness homes.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use vessel_client::pairing::{self, PreambleError};
use vessel_client::{Backoff, CallError, Connector, DeviceCredential, NodeClient};
use vessel_harness::Lookup;
use vessel_proto::{ClientInfo, ErrorCode, HarnessKind, InviteDevice, Scope};
use vessel_rpc::ClientConfig;
use vessel_rpc::memory;
use vessel_store::DataDir;

use super::local::{Local, TargetSource};
use super::mcp::FleetMcp;
use super::member::{self, Pacing};
use super::state::{ConflictWhy, StateFile};
use super::{FleetConfig, FleetFiles, FleetInit, FleetTargets};
use crate::profiles::launch::McpSource as _;
use crate::secure::SecureGateway;
use crate::{Config, Node};

/// A failed in-memory connection.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Failed(String);

/// Connects to a node's device gateway over an in-memory pair, as the
/// secure connector does over a carrier.
#[derive(Debug)]
struct MemoryConnector {
    gateway: SecureGateway,
    credential: DeviceCredential,
}

impl Connector for MemoryConnector {
    type Error = Failed;

    async fn connect(&self, client: ClientInfo) -> Result<NodeClient, Failed> {
        let (client_end, node_end) = memory::pair(NonZeroUsize::new(16).unwrap());
        let gateway = self.gateway.clone();
        drop(tokio::spawn(async move { gateway.serve(node_end).await }));
        let channel = pairing::authenticate(client_end, &self.credential)
            .await
            .map_err(|error: PreambleError| Failed(error.to_string()))?;
        NodeClient::over(channel, client, &ClientConfig::default())
            .await
            .map_err(|error| Failed(error.to_string()))
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn fast(targets: Vec<(HarnessKind, PathBuf)>) -> FleetConfig {
    FleetConfig {
        targets: FleetTargets::Only(targets),
        poll: Duration::from_millis(40),
        pull_every: Duration::from_secs(3600),
        watch_limit: Duration::from_secs(2),
    }
}

/// Wait (up to 20 s) until `ready`.
async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(20))
        .unwrap();
    while !ready() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A home: its sources, a Claude home of its own, and the node.
struct Home {
    temp: TempDir,
    node: Node,
}

impl Home {
    async fn start() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let codex = temp.path().join("codex");
        write(&codex.join("AGENTS.md"), "instructions v1\n");
        write(
            &codex.join("skills/review/SKILL.md"),
            "---\nname: review\n---\n# review\n",
        );
        let data = DataDir::new(temp.path().join("data"));
        let files = FleetFiles::new(&data);
        let _written = super::init(
            &files,
            &FleetInit {
                instructions: Some(codex.join("AGENTS.md")),
                skills: Some(codex.join("skills")),
                skip: Vec::new(),
            },
        )
        .unwrap();
        let toml = std::fs::read_to_string(files.sources()).unwrap();
        std::fs::write(
            files.sources(),
            format!(
                "{toml}\n[mcp.servers.docs]\ntype = \"stdio\"\ncommand = \"docs-mcp\"\n\
                 env = {{ DOCS_TOKEN = {{ secret = \"DOCS_TOKEN\" }} }}\n"
            ),
        )
        .unwrap();
        let claude = temp.path().join("claude");
        std::fs::create_dir_all(&claude).unwrap();
        let mut config = Config::new(data);
        config.bind = None;
        config.harness_lookup = Lookup::new(Vec::<PathBuf>::new());
        config.fleet = fast(vec![(HarnessKind::ClaudeCode, claude)]);
        let node = Node::start(config).await.unwrap();
        Self { temp, node }
    }

    fn path(&self, rest: &str) -> PathBuf {
        self.temp.path().join(rest)
    }

    fn files(&self) -> FleetFiles {
        FleetFiles::new(&DataDir::new(self.path("data")))
    }

    /// Pair a device with `scopes` through the gateway.
    async fn pair(&self, scopes: Option<Vec<Scope>>) -> DeviceCredential {
        let offer = self
            .node
            .secure
            .devices
            .invite(&InviteDevice {
                label: Some("member".to_owned()),
                ttl_secs: 600,
                scopes,
            })
            .unwrap()
            .link
            .decode()
            .unwrap();
        let (client_end, node_end) = memory::pair(NonZeroUsize::new(16).unwrap());
        let gateway = self.node.secure.clone();
        drop(tokio::spawn(async move { gateway.serve(node_end).await }));
        let (paired, _channel) = pairing::pair(client_end, &offer, "member").await.unwrap();
        paired.credential
    }

    fn connector(&self, credential: DeviceCredential) -> MemoryConnector {
        MemoryConnector {
            gateway: self.node.secure.clone(),
            credential,
        }
    }
}

/// A member's pull loop, its own data directory and a Codex home.
struct Member {
    temp: TempDir,
    local: Arc<Local>,
    links: Arc<super::Links>,
    stop: CancellationToken,
}

impl Member {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let codex = temp.path().join("codex");
        std::fs::create_dir_all(&codex).unwrap();
        let data = DataDir::new(temp.path().join("data"));
        data.prepare().unwrap();
        let store = vessel_store::Store::open(&data, vessel_store::StoreConfig::default())
            .await
            .unwrap()
            .store;
        let node = vessel_proto::NodeId::random();
        let devices = Arc::new(crate::devices::Devices::new(
            store,
            node,
            *vessel_secure::NodeKeypair::generate().unwrap().public(),
            crate::devices::Routes::default(),
        ));
        let files = FleetFiles::new(&data);
        let links = Arc::new(super::Links::new(
            node,
            "member".to_owned(),
            files.clone(),
            devices,
        ));
        let state = Arc::new(StateFile::open(&files));
        let local = Arc::new(Local::new(
            files,
            TargetSource::Only(vec![(HarnessKind::Codex, codex)]),
            state,
            Arc::new(FleetMcp::default()),
        ));
        Self {
            temp,
            local,
            links,
            stop: CancellationToken::new(),
        }
    }

    fn codex(&self, rest: &str) -> PathBuf {
        self.temp.path().join("codex").join(rest)
    }

    fn follow(&self, connector: MemoryConnector) {
        let pacing = Pacing {
            pull_every: Duration::from_secs(3600),
            backoff: Backoff {
                first: Duration::from_millis(20),
                max: Duration::from_millis(200),
            },
        };
        // No grant: these tests are about the bundle (protocol 5's part).
        let membership = member::Membership {
            home: vessel_proto::NodeId::random(),
            key: *vessel_secure::NodeKeypair::generate().unwrap().public(),
            relays: Vec::new(),
            direct: Vec::new(),
            joined_at: chrono::Utc::now(),
            grant: Vec::new(),
        };
        drop(tokio::spawn(member::run(
            connector,
            membership,
            Arc::clone(&self.local),
            Arc::clone(&self.links),
            pacing,
            self.stop.clone(),
        )));
    }
}

#[tokio::test]
async fn a_member_follows_its_home_and_reports_what_it_kept() {
    let home = Home::start().await;
    let files = home.files();
    // The files are written before the state that records them: wait for
    // the state.
    until("the home publishes and applies to itself", || {
        super::status(&files)
            .unwrap()
            .state
            .generation
            .map(vessel_proto::Generation::get)
            == Some(1)
    })
    .await;
    assert_eq!(read(&home.path("claude/CLAUDE.md")), "instructions v1\n");

    // A member, paired with the `sync` scope only.
    let member = Member::new().await;
    let credential = home.pair(Some(vec![Scope::Sync])).await;
    let device = credential.device;
    member.follow(home.connector(credential));
    until("the member gets the bundle", || {
        read(&member.codex("AGENTS.md")) == "instructions v1\n"
            && member.codex("skills/review/SKILL.md").is_file()
    })
    .await;
    // With the bundle come the MCP server definitions, values not filled:
    // this member has no DOCS_TOKEN, so sessions here would start without
    // the server and say why.
    assert_eq!(member.local.mcp.defined(), ["docs"]);
    let spec = vessel_proto::SessionSpec::new(
        HarnessKind::Codex,
        vessel_proto::SessionMode::Terminal {
            args: Vec::new(),
            size: vessel_proto::TerminalSize::default(),
        },
    );
    let servers = member.local.mcp.servers(&spec);
    assert!(servers.servers.is_empty());
    assert_eq!(servers.missing.len(), 1);
    assert_eq!(servers.missing[0].as_str(), "DOCS_TOKEN");

    // An edit on the home reaches the member through `fleet.watch`.
    write(&home.path("codex/AGENTS.md"), "instructions v2, longer\n");
    until("the member follows the edit", || {
        read(&member.codex("AGENTS.md")) == "instructions v2, longer\n"
    })
    .await;

    // The member edits its copy; the home's next change is kept beside it.
    write(&member.codex("AGENTS.md"), "my own\n");
    write(
        &home.path("codex/AGENTS.md"),
        "instructions v3, longer still\n",
    );
    until("the member applies generation 3", || {
        member
            .local
            .state
            .get()
            .generation
            .map(vessel_proto::Generation::get)
            == Some(3)
    })
    .await;
    assert_eq!(
        read(&member.codex("AGENTS.md.vessel-incoming")),
        "instructions v3, longer still\n"
    );
    assert_eq!(read(&member.codex("AGENTS.md")), "my own\n");
    let state = member.local.state.get();
    assert_eq!(state.conflicts.len(), 1, "{:?}", state.conflicts);
    assert_eq!(state.conflicts[0].kind, ConflictWhy::Edited);

    // The home lists the member with what it reported, to an admin.
    let admin = home.connector(home.pair(None).await);
    let client = admin.connect(client_info()).await.unwrap();
    let deadline = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(20))
        .unwrap();
    let listed = loop {
        let listed = client.fleet_members().await.unwrap();
        let reported = listed
            .first()
            .is_some_and(|member| member.applied.is_some_and(|g| g.get() == 3));
        if reported {
            break listed;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the report never arrived"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(listed.len(), 1, "the admin device is not a member");
    assert_eq!(listed[0].device, device);
    assert_eq!(listed[0].label, "member");
    assert_eq!(listed[0].conflicts[0].as_str(), "instructions.md");

    // A device without the `sync` scope cannot read the bundle.
    let refused = client.fleet_manifest().await.unwrap_err();
    assert!(
        matches!(&refused, CallError::Remote(error) if error.code == ErrorCode::Forbidden),
        "{refused:?}"
    );

    member.stop.cancel();
    home.node.stop().await.unwrap();
}

#[tokio::test]
async fn a_restored_file_comes_back_and_a_deleted_one_stays_away() {
    let home = Home::start().await;
    until("the home publishes", || {
        home.path("claude/CLAUDE.md").is_file()
    })
    .await;
    let member = Member::new().await;
    let credential = home.pair(Some(vec![Scope::Sync])).await;
    member.follow(home.connector(credential));
    let skill = member.codex("skills/review/SKILL.md");
    until("the member gets the skill", || skill.is_file()).await;

    // Deleted here: the next pull keeps it deleted, as a conflict.
    std::fs::remove_file(&skill).unwrap();
    write(&home.path("codex/AGENTS.md"), "instructions v2, longer\n");
    until("the member applies generation 2", || {
        member
            .local
            .state
            .get()
            .generation
            .map(vessel_proto::Generation::get)
            == Some(2)
    })
    .await;
    assert!(!skill.exists());
    let state = member.local.state.get();
    assert!(
        state
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictWhy::Deleted),
        "{:?}",
        state.conflicts
    );

    // `vessel fleet restore`: the record goes, and a pull brings it back.
    let restored = super::restore(&member.local.files, &skill).unwrap();
    assert_eq!(restored, 1);
    assert!(member.local.files.apply_request().exists());
    write(
        &home.path("codex/AGENTS.md"),
        "instructions v3, longer still\n",
    );
    until("the restored skill is written again", || skill.is_file()).await;

    member.stop.cancel();
    home.node.stop().await.unwrap();
}

fn client_info() -> ClientInfo {
    ClientInfo {
        name: "fleet-test".to_owned(),
        version: "0".to_owned(),
    }
}
