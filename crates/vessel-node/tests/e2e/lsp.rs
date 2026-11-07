//! Root grants are checked on the destination, before any server is started.

use std::path::PathBuf;

use vessel_lsp::registry::Override;
use vessel_proto::{ErrorCode, Scope, lsp};
use vessel_testkit::Fake;

use crate::agents::{as_agent, code, credential, structured, vessel_mcp};
use crate::support::{Fixture, within};

fn request(root: PathBuf) -> lsp::DocumentSymbols {
    lsp::DocumentSymbols {
        workspace: lsp::Workspace {
            root,
            language: lsp::Language::Rust,
        },
        file: lsp::FilePath::try_from(PathBuf::from("fixture.rs")).unwrap(),
        page: lsp::PageRequest::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_agent_has_its_own_cwd_but_an_owner_still_needs_a_configured_root() {
    within(async {
        let fixture = Fixture::new();
        project(&fixture.cwd());
        let tools = tempfile::tempdir().unwrap();
        let node = fixture
            .start_with(|config| {
                config.vessel_mcp = Some(vessel_mcp(&fixture));
                config.lsp.registry.rust_toolchain = Some(toolchain(&tools));
                let _previous = config.lsp.registry.overrides.insert(
                    lsp::Language::Rust,
                    Override {
                        program: Fake::Lsp.path().unwrap(),
                        args: vec!["normal".into()],
                    },
                );
            })
            .await;
        let owner = fixture.connect(&node).await;
        assert_eq!(
            code(owner.call(request(fixture.cwd())).await),
            ErrorCode::Forbidden
        );
        let mut running = structured(&owner, &fixture).await;
        let (_, socket, auth) = credential(&owner, &mut running).await;
        let agent = as_agent(&socket, &auth).await;
        let page = agent.call(request(fixture.cwd())).await.unwrap();
        assert_eq!(page.items[0].name, "target");
        let outside = tempfile::tempdir().unwrap();
        assert_eq!(
            code(agent.call(request(outside.path().to_owned())).await),
            ErrorCode::Forbidden
        );
        drop(agent);
        drop(owner);
        node.stop().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarded_lsp_reads_require_destination_roots_and_read_scope() {
    within(async {
        let relay = crate::secure::Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = crate::across::home(&relay, sources.path()).await;
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        let tools = tempfile::tempdir().unwrap();
        let target = crate::across::member_custom(
            &relay,
            &home,
            "lsp-target",
            vec![Scope::Sync],
            vec![Scope::Read],
            |config| {
                config.lsp.registry.rust_toolchain = Some(toolchain(&tools));
                config.lsp_roots.push(vessel_node::LspRoot {
                    path: root.path().to_owned(),
                    full_analysis: false,
                });
                let _previous = config.lsp.registry.overrides.insert(
                    lsp::Language::Rust,
                    Override {
                        program: Fake::Lsp.path().unwrap(),
                        args: vec!["normal".into()],
                    },
                );
            },
        )
        .await;
        let page = home
            .owner
            .call_on(target.id(), request(root.path().to_owned()))
            .await
            .unwrap();
        assert_eq!(page.items[0].name, "target");
        assert_eq!(
            code(
                home.owner
                    .call_on(target.id(), request(target.fixture.cwd()))
                    .await
            ),
            ErrorCode::Forbidden
        );
        let mut running = structured(&home.owner, &home.fixture).await;
        let (_, socket, auth) = credential(&home.owner, &mut running).await;
        let agent = as_agent(&socket, &auth).await;
        assert_eq!(
            agent
                .call_on(target.id(), request(root.path().to_owned()))
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        assert_eq!(
            code(
                agent
                    .call_on(target.id(), request(home.fixture.cwd()))
                    .await
            ),
            ErrorCode::Forbidden
        );
        let no_read = crate::across::member_granting(
            &relay,
            &home,
            "no-lsp-read",
            vec![Scope::Sync],
            vec![Scope::Operate],
        )
        .await;
        assert_eq!(
            code(
                home.owner
                    .call_on(no_read.id(), request(root.path().to_owned()))
                    .await
            ),
            ErrorCode::Forbidden
        );
        drop(agent);
        drop(no_read.owner);
        no_read.node.stop().await.unwrap();
        drop(target.owner);
        target.node.stop().await.unwrap();
        drop(home.owner);
        home.node.stop().await.unwrap();
        relay.stop().await;
    })
    .await;
}

/// A stand-in toolchain outside every root (conservative Rust pins one).
fn toolchain(dir: &tempfile::TempDir) -> PathBuf {
    vessel_testkit::rust_toolchain(dir.path()).unwrap()
}

/// A Cargo project with the fake's `fixture.rs`, so conservative Rust has a
/// checked project to link.
fn project(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
    std::fs::write(
        dir.join("fixture.rs"),
        "pub fn caller() { target(); }\npub fn target() {}\n",
    )
    .unwrap();
}
