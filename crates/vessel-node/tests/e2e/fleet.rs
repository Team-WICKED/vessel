//! Fleet sync between real nodes (plan F2): a home linked to an
//! in-process relay, and a running member that joins it through the relay
//! with a `sync` link (`vessel_node::fleet::join`, as `vessel fleet join`
//! does), notices its new membership, pulls the bundle into its harness
//! home, follows an edit made on the home, and stops after leaving; and a
//! node's sessions getting the fleet's MCP servers with that node's own
//! secrets (plan F3).

use std::path::{Path, PathBuf};
use std::time::Duration;

use vessel_node::fleet::{self, FleetFiles, FleetInit, Role};
use vessel_node::{FleetConfig, FleetTargets};
use vessel_proto::{HarnessKind, Scope};

use vessel_client::NodeClient;
use vessel_proto::{EventSeq, SessionStatus};

use crate::secure::{Relay, relayed_with};
use crate::support::{Fixture, ask, read_until, status_of, structured_spec, within};

pub(crate) fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Quick polls, and only `home` (a temporary directory) written to.
pub(crate) fn fleet_config(harness: HarnessKind, home: PathBuf) -> FleetConfig {
    std::fs::create_dir_all(&home).unwrap();
    FleetConfig {
        targets: FleetTargets::Only(vec![(harness, home)]),
        poll: Duration::from_millis(50),
        pull_every: Duration::from_secs(3600),
        watch_limit: Duration::from_secs(5),
    }
}

pub(crate) async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..800 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out: {what}");
}

#[tokio::test]
async fn a_member_joins_through_a_relay_and_follows_its_home() {
    within(async {
        let relay = Relay::start().await;
        let homes = tempfile::tempdir().unwrap();
        let agents = homes.path().join("sources/AGENTS.md");
        write(&agents, "instructions v1\n");
        write(
            &homes.path().join("sources/skills/review/SKILL.md"),
            "---\nname: review\n---\n# review\n",
        );

        let home_fixture = Fixture::new();
        let _toml = fleet::init(
            &FleetFiles::new(&home_fixture.data_dir()),
            &FleetInit {
                instructions: Some(agents.clone()),
                skills: Some(homes.path().join("sources/skills")),
                skip: Vec::new(),
            },
        )
        .unwrap();
        let home_claude = homes.path().join("home-claude");
        let home = relayed_with(&home_fixture, &[&relay.url], |config| {
            config.fleet = fleet_config(HarnessKind::ClaudeCode, home_claude.clone());
        })
        .await;
        until("the home publishes", || {
            read(&home_claude.join("CLAUDE.md")) == "instructions v1\n"
        })
        .await;

        // A running member joins with a link carrying only the relay.
        let member_codex = homes.path().join("member-codex");
        let member_fixture = Fixture::new();
        let member = member_fixture
            .start_with(|config| {
                config.fleet = fleet_config(HarnessKind::Codex, member_codex.clone());
            })
            .await;
        let owner = home_fixture.connect(&home).await;
        let offer = owner
            .invite_with_scopes(Some("laptop".to_owned()), 600, vec![Scope::Sync])
            .await
            .unwrap()
            .link
            .decode()
            .unwrap();
        assert!(offer.direct.is_empty(), "reached through the relay only");
        let member_files = FleetFiles::new(&member_fixture.data_dir());
        let joined = fleet::join(&member_files, &offer, "laptop", Vec::new())
            .await
            .unwrap();
        assert_eq!(joined.home.id, home.info().id);
        assert!(matches!(
            fleet::status(&member_files).unwrap().role,
            Role::Member { home, .. } if home == joined.home.id
        ));
        assert!(
            fleet::join(&member_files, &offer, "again", Vec::new())
                .await
                .is_err(),
            "a member does not join twice"
        );

        until("the member pulls through the relay", || {
            read(&member_codex.join("AGENTS.md")) == "instructions v1\n"
                && member_codex.join("skills/review/SKILL.md").is_file()
        })
        .await;
        write(&agents, "instructions v2, edited on the home\n");
        until("the member follows the edit", || {
            read(&member_codex.join("AGENTS.md")) == "instructions v2, edited on the home\n"
        })
        .await;
        // The report follows the files: wait for it.
        let mut members = Vec::new();
        for _ in 0..800 {
            members = owner.fleet_members().await.unwrap();
            if members[0].applied.map(vessel_proto::Generation::get) == Some(2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].label, "laptop");
        assert_eq!(
            members[0].applied.map(vessel_proto::Generation::get),
            Some(2)
        );

        // After leaving, the home's edits no longer arrive; the files stay.
        assert!(fleet::leave(&member_files).unwrap());
        tokio::time::sleep(Duration::from_millis(300)).await;
        write(&agents, "instructions v3, after the member left\n");
        until("the home applies v3 to itself", || {
            read(&home_claude.join("CLAUDE.md")) == "instructions v3, after the member left\n"
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            read(&member_codex.join("AGENTS.md")),
            "instructions v2, edited on the home\n"
        );

        member.stop().await.unwrap();
        home.stop().await.unwrap();
        relay.stop().await;
    })
    .await;
}

/// The MCP servers a new structured session's agent got, as JSON, and the
/// session's log so far.
async fn servers_of(owner: &NodeClient, fixture: &Fixture) -> (serde_json::Value, String) {
    let id = owner
        .create_session(structured_spec(fixture, None))
        .await
        .unwrap()
        .id;
    let mut stream = owner.follow(id, EventSeq::ZERO).await.unwrap();
    let mut seen = Vec::new();
    read_until(&mut stream, &mut seen, |record| {
        status_of(record) == Some(&SessionStatus::Running)
    })
    .await;
    let reply = ask(owner, &mut stream, &mut seen, id, "mcp").await;
    let servers = serde_json::from_str(reply.strip_prefix("mcp ").unwrap()).unwrap();
    let log = seen
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    owner.kill(id).await.unwrap();
    (servers, log)
}

fn server<'a>(servers: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    servers
        .as_array()?
        .iter()
        .find(|server| server["name"] == name)
}

#[tokio::test]
async fn sessions_get_the_fleets_mcp_servers_with_this_nodes_secrets() {
    within(async {
        let homes = tempfile::tempdir().unwrap();
        let agents = homes.path().join("sources/AGENTS.md");
        write(&agents, "instructions\n");
        let fixture = Fixture::new();
        let files = FleetFiles::new(&fixture.data_dir());
        let _toml = fleet::init(
            &files,
            &FleetInit {
                instructions: Some(agents),
                skills: None,
                skip: Vec::new(),
            },
        )
        .unwrap();
        let toml = std::fs::read_to_string(files.sources()).unwrap();
        std::fs::write(
            files.sources(),
            format!(
                "{toml}\n[mcp.servers.docs]\ntype = \"stdio\"\ncommand = \"docs-mcp\"\n\
                 env = {{ DOCS_TOKEN = {{ secret = \"DOCS_TOKEN\" }} }}\n\n\
                 [mcp.servers.plain]\ntype = \"stdio\"\ncommand = \"plain-mcp\"\n"
            ),
        )
        .unwrap();
        let token = "DOCS_TOKEN".parse().unwrap();
        fleet::set_secret(&files, &token, "value-of-the-secret").unwrap();
        let node = fixture
            .start_with(|config| {
                config.fleet = fleet_config(HarnessKind::ClaudeCode, homes.path().join("claude"));
            })
            .await;
        until("the home publishes", || {
            fleet::status(&files).unwrap().state.generation.is_some()
        })
        .await;
        let owner = fixture.connect(&node).await;

        let (servers, _log) = servers_of(&owner, &fixture).await;
        let docs = server(&servers, "docs").unwrap_or_else(|| panic!("no docs: {servers}"));
        let variable = docs["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variable| variable["name"] == "DOCS_TOKEN")
            .unwrap();
        assert_eq!(variable["value"], "value-of-the-secret");
        assert!(server(&servers, "plain").is_some(), "{servers}");

        // Without the secret, the next session starts without that server
        // and says why; the other server is unaffected.
        assert!(fleet::delete_secret(&files, &token).unwrap());
        // The node looks at the file every 50 ms here: a session started
        // before it did still has the server.
        let mut attempts = 0;
        let (servers, log) = loop {
            let (servers, log) = servers_of(&owner, &fixture).await;
            if server(&servers, "docs").is_none() {
                break (servers, log);
            }
            attempts += 1;
            assert!(attempts < 50, "the removed secret is still used");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(server(&servers, "plain").is_some(), "{servers}");
        assert!(
            log.contains("secret DOCS_TOKEN is not set on this node"),
            "{log}"
        );
        assert!(!log.contains("value-of-the-secret"));

        node.stop().await.unwrap();
    })
    .await;
}
