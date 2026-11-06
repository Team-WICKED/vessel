//! SSH nodes from the command line: the `ssh` runner for a node, and
//! `vessel node install` (ADR 0008).
//!
//! Which `vesseld` is installed (`--from`):
//! - `local`: a build on this machine, `--vesseld PATH` or the `vesseld`
//!   next to the `vessel` binary (how a development checkout is used:
//!   `cargo build` puts both in `target/<profile>/`). Installed as
//!   `vesseld-<version>-dev-<hash>`.
//! - `release`: this client's version from GitHub Releases, downloaded with
//!   `gh` and checked against `SHA256SUMS`. Installed as
//!   `vesseld-<version>`.
//! - `auto` (the default): `local` if there is a build for that machine
//!   next to `vessel`, else `release`. Before the first release, a
//!   development checkout installs its own build without flags.

use std::fmt::Write as _;
use std::path::PathBuf;

use vessel_client::book::Resolved;
use vessel_client::{NodeAddress, RemotePath, SshTarget};
use vessel_remote::{Build, Installed, Installer, Platform, Releases, Ssh};

use crate::args::{Install, Source, Target};
use crate::connect::data_dir;
use crate::error::CliError;
use crate::io;
use crate::nodes;

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

/// `vessel node install`.
pub(crate) async fn install(target: &Target, args: Install) -> Result<(), CliError> {
    let (resolved, _book) = nodes::resolve(target)?;
    let node = ssh_target(&resolved, "vessel node install")?;
    let ssh = ssh_for(target, node)?;
    let installed = install_on(&ssh, node, args.from, args.vesseld).await?;
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

/// Choose the build (see the module documentation) and install it.
pub(crate) async fn install_on(
    ssh: &Ssh,
    node: &SshTarget,
    from: Source,
    vesseld: Option<PathBuf>,
) -> Result<Installed, CliError> {
    let platform = Platform::detect(ssh).await?;
    let downloads = tempfile::tempdir()?;
    let build = choose(platform, from, vesseld, downloads.path()).await?;
    let data_dir = remote_data_dir(node);
    Ok(Installer::new(ssh, &data_dir)
        .install_on(platform, &build)
        .await?)
}

async fn choose(
    platform: Platform,
    from: Source,
    vesseld: Option<PathBuf>,
    downloads: &std::path::Path,
) -> Result<Build, CliError> {
    if let Some(path) = vesseld {
        return Ok(Build::local(&path).await?);
    }
    let release = || async {
        Releases::default()
            .fetch(env!("CARGO_PKG_VERSION"), platform, downloads)
            .await
    };
    match from {
        Source::Release => Ok(release().await?),
        Source::Local => Ok(Build::local(&sibling()?).await?),
        Source::Auto => {
            if let Ok(path) = sibling()
                && path.is_file()
            {
                match Build::local(&path).await {
                    Ok(build) if build.runs_on(platform) => return Ok(build),
                    Ok(build) => tracing::debug!(
                        target = build.target(),
                        %platform,
                        "the local vesseld is for another platform; using the release"
                    ),
                    Err(error) => tracing::debug!(%error, "no usable local vesseld"),
                }
            }
            Ok(release().await?)
        }
    }
}

/// The `vesseld` next to this `vessel`.
fn sibling() -> Result<PathBuf, CliError> {
    let exe = std::env::current_exe()?;
    let name = format!("vesseld{}", std::env::consts::EXE_SUFFIX);
    Ok(exe.with_file_name(name))
}
