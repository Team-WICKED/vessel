//! [`SshConnector`]: a [`Connector`] that reaches a node through `ssh` and
//! `vesseld attach` (ADR 0007, the plan's C5).
//!
//! [Decision Log]
//! - Purpose: everything that talks to a node (commands, the resuming
//!   attach) works the same over SSH as over a websocket, by dialling
//!   through this connector.
//! - Transport: `ssh … '<data_dir>/bin/<binary>' attach --data-dir
//!   '<data_dir>'`, with the protocol's length-prefixed frames
//!   (`vessel_rpc::Framed`) over the child's stdout and stdin. No client
//!   token: `attach` reads the node's own token on that machine.
//! - The exit code is the only side channel once the stream is gone
//!   (`vessel_proto::ProxyExit`), so a failed handshake waits briefly for
//!   `ssh` to exit and turns its code into a typed error:
//!   - 127: the remote shell found no such binary. With a [`Provisioner`],
//!     install and retry once; without one, say how to install.
//!   - 80 / 81 / 82: no node could start / the data directory or token is
//!     unusable / the node refused the token. Shown with `attach`'s
//!     message.
//!   - 255: `ssh` itself failed, shown with its stderr.
//! - Retrying (`Connector::is_transient`): a lost or unreachable network
//!   (255, a closed stream) and a node that could not start yet (80) are
//!   worth another try; refusals, an unusable data directory, a missing
//!   binary, a rejected login or host key are not.
//! - Handshake time: `attach` may start the node first (up to 30 s) after
//!   the SSH login, so the handshake gets 60 s by default instead of the
//!   websocket's 10 s.
//! - The `ssh` process lives as long as the connection: a task owns it and
//!   reaps it; closing the connection closes its stdin, `attach` exits, and
//!   so does `ssh`. A dead network ends it through SSH's keepalive.

use std::sync::Mutex;
use std::time::Duration;

use vessel_client::{Connector, NodeClient, RemotePath};
use vessel_proto::{ClientInfo, ProxyExit};
use vessel_rpc::{ClientConfig, Framed, FramedError, MAX_FRAME};

use crate::provision::{ProvisionError, Provisioner};
use crate::ssh::{Failed, RemoteCommand, SSH_FAILED, Ssh, SshError, StderrTail};

/// How long a failed handshake waits for `ssh` to exit with a code that
/// says why.
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// The shell's "command not found".
const NOT_FOUND: i32 = 127;

/// The handshake's default deadline over SSH (login, then possibly starting
/// the node).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Connects to a node through `ssh` and `vesseld attach`.
#[derive(Debug)]
pub struct SshConnector {
    ssh: Ssh,
    data_dir: RemotePath,
    binary: Mutex<String>,
    provisioner: Option<Provisioner>,
    config: ClientConfig,
}

/// Why [`SshConnector`] could not connect.
#[derive(Debug, thiserror::Error)]
pub enum SshConnectError {
    /// `ssh` could not be started.
    #[error(transparent)]
    Spawn(#[from] SshError),
    /// The binary is not installed and nothing may install it.
    #[error("{binary} is not installed on {host}; run `vessel node install`")]
    NotInstalled {
        /// The binary's name.
        binary: String,
        /// The machine.
        host: String,
    },
    /// `attach` exited with one of its codes (80, 81, 82).
    #[error("{}: {message}", describe(*exit))]
    Proxy {
        /// Which.
        exit: ProxyExit,
        /// `attach`'s explanation (its last stderr line).
        message: String,
    },
    /// `ssh` failed (exit 255), or the remote exited otherwise.
    #[error(transparent)]
    Failed(Failed),
    /// The stream opened but the protocol handshake failed.
    #[error("the protocol handshake over SSH failed")]
    Handshake(#[source] vessel_rpc::ConnectError<FramedError>),
    /// Installing the missing binary failed.
    #[error("installing vesseld failed")]
    Provision(#[source] ProvisionError),
}

fn describe(exit: ProxyExit) -> &'static str {
    match exit {
        ProxyExit::NodeUnavailable => "no node is running there and none could be started",
        ProxyExit::DataDirUnusable => "the node's data directory or token is unusable",
        ProxyExit::Refused => "the node refused the connection",
        ProxyExit::Closed => "the connection ended",
    }
}

impl SshConnector {
    /// Run `binary` (a name in `<data_dir>/bin`) through `ssh`. Without a
    /// provisioner, a missing binary is an error.
    #[must_use]
    pub fn new(ssh: Ssh, data_dir: RemotePath, binary: impl Into<String>) -> Self {
        let config = ClientConfig {
            handshake_timeout: HANDSHAKE_TIMEOUT,
            ..ClientConfig::default()
        };
        Self {
            ssh,
            data_dir,
            binary: Mutex::new(binary.into()),
            provisioner: None,
            config,
        }
    }

    /// Install with `provisioner` when the binary is missing, then retry
    /// once.
    #[must_use]
    pub fn with_provisioner(mut self, provisioner: Provisioner) -> Self {
        self.provisioner = Some(provisioner);
        self
    }

    /// Use these limits (the handshake timeout above all).
    #[must_use]
    pub fn with_config(mut self, config: ClientConfig) -> Self {
        self.config = config;
        self
    }

    /// The `ssh` runner.
    #[must_use]
    pub const fn ssh(&self) -> &Ssh {
        &self.ssh
    }

    /// The node's data directory on its machine.
    #[must_use]
    pub const fn data_dir(&self) -> &RemotePath {
        &self.data_dir
    }

    /// The binary it runs now (a provisioner may have changed it).
    #[must_use]
    pub fn binary(&self) -> String {
        self.binary
            .lock()
            .map(|binary| binary.clone())
            .unwrap_or_default()
    }

    async fn dial(&self, client: ClientInfo) -> Result<NodeClient, SshConnectError> {
        let binary = self
            .data_dir
            .join("bin")
            .and_then(|bin| bin.join(&self.binary()))
            .map_err(|_invalid| SshConnectError::NotInstalled {
                binary: self.binary(),
                host: self.ssh.target().host.clone(),
            })?;
        let command = RemoteCommand::path_program(&binary)
            .arg("attach")
            .arg("--data-dir")
            .path(&self.data_dir);
        let mut running = self.ssh.spawn(&command)?;
        let stderr = running.stderr_handle();
        let (Some(stdout), Some(stdin)) = (running.child.stdout.take(), running.child.stdin.take())
        else {
            return Err(SshError::Io(std::io::Error::other("ssh's stdio is not piped")).into());
        };
        let transport = Framed::new(stdout, stdin, MAX_FRAME);
        match NodeClient::over(transport, client, &self.config).await {
            Ok(connection) => {
                let host = self.ssh.target().host.clone();
                let mut child = running.child;
                drop(tokio::spawn(async move {
                    let status = child.wait().await;
                    tracing::debug!(%host, ?status, stderr = ?stderr.snapshot().last(), "ssh ended");
                }));
                Ok(connection)
            }
            Err(error) => {
                let status = tokio::time::timeout(EXIT_WAIT, running.child.wait()).await;
                let code = match status {
                    Ok(Ok(status)) => status.code(),
                    // Still running or not waitable: the handshake error
                    // itself is the reason.
                    Ok(Err(_)) | Err(_) => return Err(SshConnectError::Handshake(error)),
                };
                Err(classify(code, stderr.snapshot(), error, self))
            }
        }
    }
}

/// The error for an `ssh` that exited with `code` before the handshake
/// finished.
fn classify(
    code: Option<i32>,
    stderr: StderrTail,
    handshake: vessel_rpc::ConnectError<FramedError>,
    connector: &SshConnector,
) -> SshConnectError {
    let message = stderr.last().unwrap_or_default().to_owned();
    match code {
        Some(NOT_FOUND) => SshConnectError::NotInstalled {
            binary: connector.binary(),
            host: connector.ssh.target().host.clone(),
        },
        Some(code) => match ProxyExit::from_code(code) {
            Some(ProxyExit::Closed) => SshConnectError::Handshake(handshake),
            Some(exit) => SshConnectError::Proxy { exit, message },
            None => SshConnectError::Failed(Failed {
                code: Some(code),
                stderr,
            }),
        },
        None => SshConnectError::Failed(Failed { code, stderr }),
    }
}

impl Connector for SshConnector {
    type Error = SshConnectError;

    async fn connect(&self, client: ClientInfo) -> Result<NodeClient, SshConnectError> {
        match self.dial(client.clone()).await {
            Err(SshConnectError::NotInstalled { binary, host }) => {
                let Some(provisioner) = &self.provisioner else {
                    return Err(SshConnectError::NotInstalled { binary, host });
                };
                tracing::info!(%host, %binary, "vesseld is not installed there; installing it");
                let installed = provisioner
                    .install(&self.ssh, &self.data_dir)
                    .await
                    .map_err(SshConnectError::Provision)?;
                if let Some(name) = installed.binary.as_str().rsplit('/').next()
                    && let Ok(mut binary) = self.binary.lock()
                {
                    name.clone_into(&mut binary);
                }
                self.dial(client).await
            }
            other => other,
        }
    }

    fn is_transient(&self, error: &SshConnectError) -> bool {
        match error {
            SshConnectError::Failed(failed) => {
                failed.code == Some(SSH_FAILED)
                    && !failed.stderr.contains("Permission denied")
                    && !failed.stderr.contains("Host key verification failed")
            }
            SshConnectError::Proxy { exit, .. } => *exit == ProxyExit::NodeUnavailable,
            SshConnectError::Handshake(error) => matches!(
                error,
                vessel_rpc::ConnectError::Transport(_)
                    | vessel_rpc::ConnectError::Timeout
                    | vessel_rpc::ConnectError::Closed
            ),
            SshConnectError::Spawn(_)
            | SshConnectError::NotInstalled { .. }
            | SshConnectError::Provision(_) => false,
        }
    }
}
