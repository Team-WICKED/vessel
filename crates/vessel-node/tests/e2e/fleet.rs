//! Fleet sync between real nodes (plan F2): a home linked to an
//! in-process relay, and a running member that joins it through the relay
//! with a `sync` link (`vessel_node::fleet::join`, as `vessel fleet join`
//! does), notices its new membership, pulls the bundle into its harness
//! home, follows an edit made on the home, and stops after leaving.

use std::path::{Path, PathBuf};
use std::time::Duration;

use vessel_node::fleet::{self, FleetFiles, FleetInit, Role};
use vessel_node::{FleetConfig, FleetTargets};
use vessel_proto::{HarnessKind, Scope};

use crate::secure::{Relay, relayed_with};
use crate::support::{Fixture, within};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Quick polls, and only `home` (a temporary directory) written to.
fn fleet_config(harness: HarnessKind, home: PathBuf) -> FleetConfig {
    std::fs::create_dir_all(&home).unwrap();
    FleetConfig {
        targets: FleetTargets::Only(vec![(harness, home)]),
        poll: Duration::from_millis(50),
        pull_every: Duration::from_secs(3600),
        watch_limit: Duration::from_secs(5),
    }
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
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
        let joined = fleet::join(&member_files, &offer, "laptop").await.unwrap();
        assert_eq!(joined.home.id, home.info().id);
        assert!(matches!(
            fleet::status(&member_files).unwrap().role,
            Role::Member { home, .. } if home == joined.home.id
        ));
        assert!(
            fleet::join(&member_files, &offer, "again").await.is_err(),
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
