//! SSH nodes from the command line (ADRs 0007 and 0008): the `ssh` runner
//! and connector for a node, `vessel node install`, `vessel node update`.
//!
//! Which `vesseld` a machine gets (`--from`, `vessel_remote::Provisioner`):
//! - `local`: a build on this machine, `--vesseld PATH` or the `vesseld`
//!   next to the `vessel` binary (how a development checkout is used:
//!   `cargo build` puts both in `target/<profile>/`), installed as
//!   `vesseld-<version>-dev-<hash>`;
//! - `release`: this client's version from GitHub Releases, downloaded with
//!   `gh` and checked against `SHA256SUMS`, installed as
//!   `vesseld-<version>`;
//! - `auto` (the default): `local` if there is a build for that machine
//!   next to `vessel`, else `release`.
//!
//! Connecting installs on demand: a missing binary (exit 127) is installed
//! from `auto` and the connection retried once.

use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::PathBuf;

use vessel_client::book::Resolved;
use vessel_client::{CallError, NodeAddress, RemotePath, SshTarget};
use vessel_proto::{ErrorCode, SessionInfo, SessionMode, StopWhen};
use vessel_remote::{Provisioner, Releases, Source as BuildSource, Ssh, SshConnector};

use crate::args::{Install, Source, Target, Update};
use crate::connect::{self, data_dir};
use crate::error::CliError;
use crate::{io, nodes, render};

/// The `ssh` runner for `node`: the system `ssh` (or `VESSEL_SSH`, for
/// tests and unusual setups), sharing logins through `<data_dir>/ssh` on
/// Unix.
pub(crate) fn ssh_for(target: &Target, node: &SshTarget) -> Result<Ssh, CliError> {
    let mut ssh = Ssh::new(node.clone()).with_control_dir(data_dir(target)?.root().join("ssh"));
    if let Some(program) = std::env::var_os("VESSEL_SSH") {
        ssh = ssh.with_program(program);
    }
    Ok(ssh)
}

/// The node's data directory on its machine.
pub(crate) fn remote_data_dir(node: &SshTarget) -> RemotePath {
    node.data_dir
        .clone()
        .unwrap_or_else(RemotePath::default_data_dir)
}

/// Where installs come from: `--vesseld`, else `--from`.
pub(crate) fn provisioner(from: Source, vesseld: Option<PathBuf>) -> Result<Provisioner, CliError> {
    let mut releases = Releases::default();
    if let Some(gh) = std::env::var_os("VESSEL_GH") {
        releases = releases.with_program(gh);
    }
    let source = match (vesseld, from) {
        (Some(path), _) => BuildSource::Local(path),
        (None, Source::Local) => BuildSource::Local(sibling()?),
        (None, Source::Release) => BuildSource::Release(releases),
        (None, Source::Auto) => BuildSource::Auto {
            local: sibling().ok(),
            releases,
        },
    };
    Ok(Provisioner::new(source, env!("CARGO_PKG_VERSION")))
}

/// The connector for an SSH node, installing on demand from `auto`.
pub(crate) async fn connector(target: &Target, node: &SshTarget) -> Result<SshConnector, CliError> {
    let provisioner = provisioner(Source::Auto, None)?;
    let binary = provisioner.expected_binary().await;
    Ok(
        SshConnector::new(ssh_for(target, node)?, remote_data_dir(node), binary)
            .with_provisioner(provisioner),
    )
}

/// The SSH side of `resolved`, or an error naming `what` needs it.
pub(crate) fn ssh_target<'a>(
    resolved: &'a Resolved,
    what: &'static str,
) -> Result<&'a SshTarget, CliError> {
    match &resolved.address {
        NodeAddress::Ssh(node) => Ok(node),
        NodeAddress::Ws(_) => Err(CliError::NotSsh {
            node: nodes::label(resolved),
            what,
        }),
    }
}

/// `vessel node install`.
pub(crate) async fn install(target: &Target, args: Install) -> Result<(), CliError> {
    let (resolved, _book) = nodes::resolve(target)?;
    let node = ssh_target(&resolved, "vessel node install")?;
    let ssh = ssh_for(target, node)?;
    let installed = provisioner(args.from, args.vesseld)?
        .install(&ssh, &remote_data_dir(node))
        .await?;
    let label = nodes::label(&resolved);
    let mut text = if installed.uploaded {
        format!("installed {} on {label}\n", installed.binary)
    } else {
        format!("{} is already installed on {label}\n", installed.binary)
    };
    for removed in &installed.removed {
        let _infallible = writeln!(text, "removed {removed}");
    }
    Ok(io::out(text.as_bytes()).await?)
}

/// `vessel node update`: install this version, then restart the node into
/// it, as the live sessions allow (ADR 0008). A node already running this
/// release is left alone; with a development build (`-dev-<hash>`, whose
/// version string is shared by every build) the restart always happens.
///
/// - no live session: stop it now; the next connection starts the new
///   version;
/// - live sessions and no flag: list them and change nothing;
/// - `--when-idle`: the node drains (refuses new sessions, stops after the
///   last one ends);
/// - `--now`: after a confirmation, stop it now; live sessions end
///   `Interrupted`, and the structured ones can be resumed (printed).
pub(crate) async fn update(target: &Target, args: Update) -> Result<(), CliError> {
    let (resolved, book) = nodes::resolve(target)?;
    let node = ssh_target(&resolved, "vessel node update")?;
    let label = nodes::label(&resolved);
    let ssh = ssh_for(target, node)?;
    let installed = provisioner(args.from, args.vesseld)?
        .install(&ssh, &remote_data_dir(node))
        .await?;
    if installed.uploaded {
        io::note(&format!(
            "[vessel] installed {} on {label}\n",
            installed.binary
        ))
        .await?;
    }
    let connected = connect::connect_to(target, &resolved, &book).await?;
    let client = &connected.client;
    let running = client.node().version.clone();
    // A node of this version runs this release: nothing to do. Development
    // builds share their version string, so whether the node runs this one
    // cannot be told from it; for those `update` always restarts.
    let development = installed.binary.as_str().contains("-dev-");
    if !development && !installed.uploaded && running == env!("CARGO_PKG_VERSION") {
        let text = format!("{label} already runs vesseld {running}\n");
        return Ok(io::out(text.as_bytes()).await?);
    }
    let live: Vec<SessionInfo> = client
        .sessions()
        .await?
        .into_iter()
        .filter(|session| session.status.is_live())
        .collect();
    let when = if live.is_empty() {
        StopWhen::Now
    } else if args.when_idle {
        StopWhen::WhenIdle
    } else if args.now {
        if !args.yes && !confirm(&label, live.len()).await? {
            return Err(CliError::Usage("not confirmed; nothing was stopped"));
        }
        StopWhen::Now
    } else {
        io::out(live_list(&label, &live).as_bytes()).await?;
        return Err(CliError::LiveSessions(live.len()));
    };
    match client.stop_node(when).await {
        // The node closes every connection as it stops; the answer may
        // not make it back.
        Ok(()) | Err(CallError::Disconnected) => {}
        Err(error) if error.code() == Some(ErrorCode::Unsupported) => {
            return Err(CliError::TooOldToStop(running));
        }
        Err(error) => return Err(error.into()),
    }
    let mut text = match when {
        StopWhen::Now => format!(
            "stopped vesseld {running} on {label}; the next connection starts {}\n",
            installed.binary
        ),
        StopWhen::WhenIdle => format!(
            "{label} is draining: it refuses new sessions and stops when the last of its {} live \
             session(s) ends; the next connection then starts {}\n",
            live.len(),
            installed.binary
        ),
    };
    if when == StopWhen::Now {
        for session in &live {
            if let (SessionMode::Structured { .. }, Some(harness_session)) =
                (&session.spec.mode, &session.harness_session)
            {
                let _infallible = writeln!(
                    text,
                    "resume {}: vessel --node {label} run {} --structured --resume {harness_session}",
                    session.id, session.spec.harness
                );
            }
        }
    }
    Ok(io::out(text.as_bytes()).await?)
}

/// The live sessions that keep `update` from stopping the node.
fn live_list(label: &str, live: &[SessionInfo]) -> String {
    let mut text = format!(
        "{label} has {} live session(s); nothing was changed:\n",
        live.len()
    );
    for session in live {
        let _infallible = writeln!(
            text,
            "  {}  {:<12} {:<12} {}",
            session.id,
            session.spec.harness,
            render::mode(&session.spec.mode),
            render::status(&session.status)
        );
    }
    text.push_str(
        "stop it once they end with --when-idle, or now with --now (they end interrupted)\n",
    );
    text
}

/// Ask before interrupting `count` sessions; only a person at a terminal
/// can answer.
async fn confirm(label: &str, count: usize) -> Result<bool, CliError> {
    if !std::io::stdin().is_terminal() {
        return Err(CliError::Usage(
            "--now interrupts live sessions; confirm with --yes when not at a terminal",
        ));
    }
    io::note(&format!(
        "stop {label} now, interrupting {count} live session(s)? [y/N] "
    ))
    .await?;
    let mut lines = io::input_lines()?;
    let answer = lines.recv().await.unwrap_or_default();
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// The `vesseld` next to this `vessel`.
fn sibling() -> Result<PathBuf, CliError> {
    let exe = std::env::current_exe()?;
    let name = format!("vesseld{}", std::env::consts::EXE_SUFFIX);
    Ok(exe.with_file_name(name))
}
