//! The node book from the command line, and `vessel node install`.

use crate::support::{Fixture, within};

/// The node book: a websocket node added by name becomes the default, its
/// id is recorded on first contact, and a different node answering later
/// is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_nodes_are_added_used_and_pinned() {
    within(async {
        let fixture = Fixture::start().await;
        let dir = fixture.data();
        let url = format!("ws://{}", fixture.node_address());
        let token = fixture.data().join("token");
        let token = token.to_str().unwrap();
        let added = fixture
            .vessel_book(
                dir,
                &[
                    "nodes",
                    "add",
                    "here",
                    &url,
                    "--token-file",
                    token,
                    "--default",
                ],
                &[],
            )
            .await;
        assert!(added.ok, "{}", added.stderr);
        let list = fixture.vessel_book(dir, &["nodes", "list"], &[]).await;
        assert!(list.stdout.contains("* here"), "{}", list.stdout);
        let before = fixture
            .vessel_book(dir, &["nodes", "show", "here"], &[])
            .await;
        assert!(before.stdout.contains("not seen yet"), "{}", before.stdout);

        // No --node: the book's default, with its token file.
        let status = fixture.vessel_book(dir, &["status"], &[]).await;
        assert!(status.ok, "{}", status.stderr);
        assert!(status.stdout.contains("claude-code"), "{}", status.stdout);
        let shown = fixture
            .vessel_book(dir, &["nodes", "show", "here"], &[])
            .await;
        let id = shown
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .unwrap()
            .to_owned();
        assert!(
            id.parse::<vessel_proto::NodeId>().is_ok(),
            "{}",
            shown.stdout
        );

        // Another node answering at that address is refused.
        let book = std::fs::read_to_string(fixture.book()).unwrap();
        let other = vessel_proto::NodeId::random().to_string();
        std::fs::write(fixture.book(), book.replace(&id, &other)).unwrap();
        let refused = fixture.vessel_book(dir, &["status"], &[]).await;
        assert!(!refused.ok);
        assert!(
            refused.stderr.contains("a different node answered"),
            "{}",
            refused.stderr
        );

        let removed = fixture
            .vessel_book(dir, &["nodes", "remove", "here"], &[])
            .await;
        assert!(removed.ok, "{}", removed.stderr);
        let empty = fixture.vessel_book(dir, &["nodes", "list"], &[]).await;
        assert!(empty.stdout.contains("no nodes yet"), "{}", empty.stdout);
        fixture.stop().await;
    })
    .await;
}

/// A project's `.vessel/config.toml` picks one of the user's nodes, and
/// may do nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_project_file_only_picks_among_the_users_nodes() {
    within(async {
        let fixture = Fixture::start().await;
        let url = format!("ws://{}", fixture.node_address());
        let token = fixture.data().join("token");
        let added = fixture
            .vessel_book(
                fixture.data(),
                &[
                    "nodes",
                    "add",
                    "lab",
                    &url,
                    "--token-file",
                    token.to_str().unwrap(),
                ],
                &[],
            )
            .await;
        assert!(added.ok, "{}", added.stderr);

        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("src");
        std::fs::create_dir_all(project.path().join(".vessel")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let config = project.path().join(".vessel").join("config.toml");
        std::fs::write(&config, "default_node = \"lab\"\n").unwrap();
        let status = fixture.vessel_book(&nested, &["status"], &[]).await;
        assert!(status.ok, "{}", status.stderr);

        std::fs::write(&config, "[nodes.evil]\nssh = \"attacker.example\"\n").unwrap();
        let refused = fixture.vessel_book(&nested, &["status"], &[]).await;
        assert!(!refused.ok);
        assert!(
            refused.stderr.contains("may only set `default_node`"),
            "{}",
            refused.stderr
        );
        std::fs::write(&config, "default_node = \"elsewhere\"\n").unwrap();
        let unknown = fixture.vessel_book(&nested, &["status"], &[]).await;
        assert!(!unknown.ok);
        assert!(
            unknown.stderr.contains("not a node in your book"),
            "{}",
            unknown.stderr
        );
        fixture.stop().await;
    })
    .await;
}

/// `vessel node install` uploads a local build to an SSH node and names it
/// by content; a second install finds it. Through `fake-ssh`, which runs
/// the command in a temporary "remote" home, and a stand-in `vesseld` (a
/// shell script: Unix only).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_install_uploads_a_local_build_over_ssh() {
    within(async {
        let fixture = Fixture::start().await;
        let remote = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let ssh = vessel_testkit::Fake::Ssh.path().unwrap();
        let os = if cfg!(target_os = "macos") {
            "apple-darwin"
        } else {
            "unknown-linux-gnu"
        };
        let triple = format!("{}-{os}", std::env::consts::ARCH);
        let vesseld = tools.path().join("vesseld");
        crate::support::executable(
            &vesseld,
            &format!(
                "#!/bin/sh\ncase \"$1\" in version) echo '0.2.0 {triple}' ;; cleanup) ;; *) exit 2 ;; esac\n"
            ),
        );
        let env = [
            ("VESSEL_SSH", ssh.as_os_str()),
            ("FAKE_SSH_HOME", remote.path().as_os_str()),
        ];
        let dir = fixture.data();
        let added = fixture
            .vessel_book(dir, &["nodes", "add", "gpu", "me@gpu-box"], &env)
            .await;
        assert!(added.ok, "{}", added.stderr);
        let args = [
            "--node",
            "gpu",
            "node",
            "install",
            "--vesseld",
            vesseld.to_str().unwrap(),
        ];
        let installed = fixture.vessel_book(dir, &args, &env).await;
        assert!(installed.ok, "{}", installed.stderr);
        assert!(
            installed.stdout.contains("installed ~/.vessel/bin/vesseld-0.2.0-dev-")
                && installed.stdout.contains(" on gpu"),
            "{}",
            installed.stdout
        );
        let bin = remote.path().join(".vessel").join("bin");
        let names: Vec<_> = std::fs::read_dir(&bin)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].starts_with("vesseld-0.2.0-dev-"), "{names:?}");

        let again = fixture.vessel_book(dir, &args, &env).await;
        assert!(again.ok, "{}", again.stderr);
        assert!(again.stdout.contains("already installed"), "{}", again.stdout);

        let ws = format!("ws://{}", fixture.node_address());
        let not_ssh = fixture
            .vessel_book(dir, &["--node", &ws, "node", "install"], &env)
            .await;
        assert!(!not_ssh.ok);
        assert!(
            not_ssh.stderr.contains("is not an SSH node"),
            "{}",
            not_ssh.stderr
        );
        fixture.stop().await;
    })
    .await;
}
