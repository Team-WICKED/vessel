//! `SshConnector` against the real `vesseld` (`vessel_testkit::vesseld`)
//! through `fake-ssh`: install on first contact, the node started by
//! `attach`, exit codes, and the node stopped again.

use std::time::Duration;

use vessel_client::{Connector as _, NodeClient};
use vessel_proto::{ClientInfo, StopWhen};
use vessel_store::{DataDir, NodeLock};
use vessel_testkit::Fake;

use super::*;
use crate::connector::{SshConnectError, SshConnector};
use crate::provision::{Provisioner, Source};
use crate::release::Releases;
use crate::ssh::Ssh;

const LIMIT: Duration = Duration::from_secs(60);

fn client_info() -> ClientInfo {
    ClientInfo {
        name: "vessel-remote-tests".to_owned(),
        version: "0".to_owned(),
    }
}

/// A remote reached through `fake-ssh`; stops any node it ends up running.
struct Machine {
    remote: Remote,
}

impl Machine {
    fn new() -> Self {
        Self {
            remote: Remote::new(),
        }
    }

    fn ssh(&self) -> Ssh {
        Ssh::new("gpu-box".parse().unwrap())
            .with_program(Fake::Ssh.path().unwrap())
            .with_env("FAKE_SSH_HOME", self.remote.home.path())
    }

    fn data_dir(&self) -> DataDir {
        DataDir::new(self.remote.home.path().join(".vessel"))
    }

    /// Stop the node through `client`, and wait until it let go of its
    /// data directory.
    async fn stop(&self, client: &NodeClient) {
        drop(client.stop_node(StopWhen::Now).await);
        let lock = self.data_dir().lock_path();
        tokio::task::spawn_blocking(move || {
            std::fs::File::open(lock).unwrap().lock().unwrap();
        })
        .await
        .unwrap();
    }
}

impl Drop for Machine {
    /// A node left running by a failed test would outlive its home.
    fn drop(&mut self) {
        if let Ok(Some(owner)) = NodeLock::holder(&self.data_dir()) {
            drop(
                std::process::Command::new("kill")
                    .arg(owner.pid.to_string())
                    .status(),
            );
        }
    }
}

#[tokio::test]
async fn a_fresh_machine_gets_vesseld_and_a_node_on_first_contact() {
    let vesseld = vessel_testkit::vesseld().unwrap();
    let machine = Machine::new();
    tokio::time::timeout(LIMIT, async {
        let provisioner = Provisioner::new(Source::Local(vesseld.clone()), "0.0.0");
        let binary = provisioner.expected_binary().await;
        assert!(binary.contains("-dev-"), "{binary}");
        let connector = SshConnector::new(machine.ssh(), Remote::data_dir(), binary.clone())
            .with_provisioner(provisioner);
        let client = connector.connect(client_info()).await.unwrap();
        assert_eq!(client.protocol(), vessel_proto::PROTOCOL_VERSION);
        assert!(client.harnesses().await.is_ok());
        assert_eq!(machine.remote.installed(), vec![binary.clone()]);

        // A second connection finds both the binary and the node.
        let second = connector.connect(client_info()).await.unwrap();
        assert_eq!(second.node().id, client.node().id);
        machine.stop(&second).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn without_a_provisioner_a_missing_binary_says_how_to_install() {
    let machine = Machine::new();
    tokio::time::timeout(LIMIT, async {
        let connector = SshConnector::new(machine.ssh(), Remote::data_dir(), "vesseld-9.9.9");
        let error = connector.connect(client_info()).await.unwrap_err();
        assert!(
            matches!(&error, SshConnectError::NotInstalled { binary, .. } if binary == "vesseld-9.9.9"),
            "{error:?}"
        );
        assert!(error.to_string().contains("vessel node install"), "{error}");
        assert!(!connector.is_transient(&error));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ssh_failures_are_transient_and_keep_their_stderr() {
    let machine = Machine::new();
    tokio::time::timeout(LIMIT, async {
        // fake-ssh exits 255 when it cannot enter the "remote" home.
        let ssh = machine.ssh().with_env("FAKE_SSH_HOME", "/nonexistent/home");
        let connector = SshConnector::new(ssh, Remote::data_dir(), "vesseld-0.0.0");
        let error = connector.connect(client_info()).await.unwrap_err();
        assert!(matches!(error, SshConnectError::Failed(_)), "{error:?}");
        assert!(connector.is_transient(&error), "{error:?}");
    })
    .await
    .unwrap();
}

/// `Auto` without a local build fetches the release; a stand-in `gh` that
/// fails shows why.
#[tokio::test]
async fn auto_without_a_local_build_falls_back_to_the_release() {
    let machine = Machine::new();
    tokio::time::timeout(LIMIT, async {
        let gh = machine.remote.local().join("gh");
        executable(&gh, "#!/bin/sh\necho 'release not found' >&2\nexit 1\n");
        let provisioner = Provisioner::new(
            Source::Auto {
                local: None,
                releases: Releases::default().with_program(&gh),
            },
            "0.2.0",
        );
        assert_eq!(provisioner.expected_binary().await, "vesseld-0.2.0");
        let connector = SshConnector::new(machine.ssh(), Remote::data_dir(), "vesseld-0.2.0")
            .with_provisioner(provisioner);
        let error = connector.connect(client_info()).await.unwrap_err();
        assert!(matches!(error, SshConnectError::Provision(_)), "{error:?}");
        let chain = std::error::Error::source(&error).unwrap().to_string();
        assert!(chain.contains("release not found"), "{chain}");
    })
    .await
    .unwrap();
}
