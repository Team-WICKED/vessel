//! One function per command. Each connects, does its calls, and prints;
//! formatting lives in `render` and `view`.

use std::net::SocketAddr;
use std::path::PathBuf;

use bytes::Bytes;
use vessel_harness::HarnessEnv;
use vessel_node::{Config, Node, VesselMcp};
use vessel_proto::{
    ContentBlock, EventSeq, HarnessKind, HarnessSessionId, ModelId, ProfileName, SessionInput,
    SessionMode, SessionSpec, TerminalBytes, TerminalInput,
};

use vessel_client::book::Resolved;

use crate::args::{Cli, Command, NodeCommand, Run, SessionCommand, Target};
use crate::attach::{self, terminal};
use crate::connect::{connect, connect_to, data_dir, is_here};
use crate::error::CliError;
use crate::selector::SessionSelector;
use crate::{devices, fleet, init, io, nodes, remote, render, transfer, ui};

pub(crate) async fn run(cli: Cli) -> Result<(), CliError> {
    let target = &cli.target;
    match cli.command {
        Command::Node {
            command: NodeCommand::Start { bind, name },
        } => node_start(target, bind, name).await,
        Command::Node {
            command: NodeCommand::Install(install),
        } => remote::install(target, install).await,
        Command::Node {
            command: NodeCommand::Update(update),
        } => remote::update(target, update).await,
        Command::Node {
            command: NodeCommand::Status,
        } => {
            let client = connect(target).await?.client;
            let node = client.node_info().await?;
            let sessions = client.sessions().await?;
            let text = render::node_status(&node, &sessions, env!("CARGO_PKG_VERSION"));
            Ok(io::out(text.as_bytes()).await?)
        }
        Command::Node {
            command: NodeCommand::Pair { label, ttl },
        } => devices::pair(target, label, ttl).await,
        Command::Node {
            command: NodeCommand::Devices { command },
        } => devices::devices(target, command).await,
        Command::Nodes { command } => nodes::command(target, command).await,
        Command::Fleet { command } => Box::pin(fleet::command(target, command)).await,
        Command::Status => {
            let client = connect(target).await?.client;
            let node = client.node_info().await?;
            let harnesses = client.harnesses().await?;
            let text = format!("{}\n{}", render::node(&node), render::harnesses(&harnesses));
            Ok(io::out(text.as_bytes()).await?)
        }
        Command::Harnesses => {
            let client = connect(target).await?.client;
            let text = render::harnesses(&client.harnesses().await?);
            Ok(io::out(text.as_bytes()).await?)
        }
        Command::Profiles { harness, reload } => Box::pin(profiles(target, harness, reload)).await,
        Command::Sessions => {
            let client = connect(target).await?.client;
            let text = render::sessions(&client.sessions().await?);
            Ok(io::out(text.as_bytes()).await?)
        }
        Command::Run(run) => run_session(target, run).await,
        Command::Attach { session, from } => {
            let connected = connect(target).await?;
            let info = session.resolve(&connected.client).await?;
            attach::attach(&connected, &info, from).await
        }
        Command::Send { session, text } => send_text(target, &session, text).await,
        Command::Log { session, from } => log(target, &session, from).await,
        Command::Kill { session } => {
            let client = connect(target).await?.client;
            let info = session.resolve(&client).await?;
            Ok(client.kill(info.id).await?)
        }
        Command::Mcp => Ok(Box::pin(vessel_mcp::run_stdio()).await?),
        Command::Init(args) => {
            let options = init::Options {
                service: args.service,
                join: args.join,
                consent: init::Consent::new(args.yes, args.install),
                home: args.home.or_else(dirs::home_dir).ok_or(CliError::NoHome)?,
            };
            Box::pin(init::run(data_dir(target)?, options)).await
        }
        Command::Session {
            command:
                SessionCommand::MoveProfile {
                    session,
                    profile,
                    force,
                },
        } => Box::pin(move_profile(target, &session, profile, force)).await,
        Command::Session {
            command: SessionCommand::Copy { to, start },
        } => Box::pin(transfer::copy_session(target, to, start, false)).await,
        Command::Session {
            command: SessionCommand::Move { to },
        } => Box::pin(transfer::copy_session(target, to, true, true)).await,
        Command::Ui(args) => Box::pin(ui::ui(args)).await,
    }
}

/// `vessel session move-profile`: the node moves the session; print where
/// it runs now.
async fn move_profile(
    target: &Target,
    session: &SessionSelector,
    profile: ProfileName,
    force: bool,
) -> Result<(), CliError> {
    let client = connect(target).await?.client;
    let info = session.resolve(&client).await?;
    let moved = client.move_profile(info.id, Some(profile), force).await?;
    let line = format!(
        "{} now runs under {}\n",
        moved.id,
        render::profile(moved.profile.as_ref())
    );
    Ok(io::out(line.as_bytes()).await?)
}

/// `vessel profiles`: list them, after a reload if asked.
async fn profiles(
    target: &Target,
    harness: Option<HarnessKind>,
    reload: bool,
) -> Result<(), CliError> {
    let client = connect(target).await?.client;
    let profiles = if reload {
        let reloaded = client.reload_profiles().await?;
        match harness {
            Some(harness) => reloaded
                .into_iter()
                .filter(|profile| profile.harness == harness)
                .collect(),
            None => reloaded,
        }
    } else {
        client.profiles(harness).await?
    };
    Ok(io::out(render::profiles(&profiles).as_bytes()).await?)
}

async fn node_start(
    target: &Target,
    bind: SocketAddr,
    name: Option<String>,
) -> Result<(), CliError> {
    // The binary, so the account it runs as comes from its own environment
    // (`HarnessEnv`; the library never reads it).
    let env = HarnessEnv::from_vars(dirs::home_dir().ok_or(CliError::NoHome)?, |name| {
        std::env::var_os(name)
    });
    let mut config = Config::new(data_dir(target)?, env);
    config.bind = Some(bind);
    config.name = name;
    // Sessions' agents reach this node through this executable's `mcp`
    // command (ADR 0013).
    config.vessel_mcp = std::env::current_exe().ok().map(VesselMcp::vessel);
    let node = Node::start(config).await?;
    let address = node.address().unwrap_or(bind);
    let started = format!(
        "[vessel] node {} listening on {address}; Ctrl-C stops it\n",
        node.info().name,
    );
    io::note(&started).await?;
    // A client's `node.stop` (protocol 3) stops it too.
    tokio::select! {
        () = vessel_node::signal() => {}
        () = node.stop_requested() => {}
    }
    io::note("[vessel] stopping\n").await?;
    Ok(node.stop().await?)
}

async fn run_session(target: &Target, run: Run) -> Result<(), CliError> {
    // The spec is checked before connecting, so a bad `--cwd` fails without
    // touching the network.
    let mode = if run.structured {
        SessionMode::Structured {
            model: run.model.map(ModelId::new),
            resume: run.resume.map(HarnessSessionId::new),
        }
    } else {
        SessionMode::Terminal {
            args: run.args,
            size: terminal::window_size().unwrap_or_default(),
        }
    };
    let (resolved, book) = nodes::resolve(target)?;
    let mut spec = SessionSpec::new(run.harness, mode);
    spec.cwd = run.cwd.map(|cwd| node_cwd(&resolved, cwd)).transpose()?;
    spec.profile = run.profile;
    let connected = connect_to(target, &resolved, &book).await?;
    let info = connected.client.create_session(spec).await?;
    if run.detach {
        return Ok(io::out(format!("{}\n", info.id).as_bytes()).await?);
    }
    io::note(&format!("[vessel] session {}\n", info.id)).await?;
    attach::attach(&connected, &info, EventSeq::ZERO).await
}

/// `--cwd` as the node needs it: an absolute path on the node's machine.
///
/// The node refuses relative directories (they would be resolved against
/// wherever `vesseld` happens to run). A relative `--cwd` means "relative
/// to where I am", which only has a meaning when the node runs on this
/// machine: then it is resolved against this process's current directory.
/// For a node on another machine there is no such place, so it is refused
/// here with an error saying so, instead of being sent and failing there
/// with a less helpful one. "This machine" is a loopback websocket node, as
/// for the token file (`connect`); an SSH node is another machine.
fn node_cwd(resolved: &Resolved, cwd: PathBuf) -> Result<PathBuf, CliError> {
    if cwd.is_absolute() {
        return Ok(cwd);
    }
    if !is_here(resolved) {
        return Err(CliError::RelativeCwd {
            cwd,
            node: nodes::label(resolved),
        });
    }
    Ok(std::path::absolute(&cwd)?)
}

/// A line for a terminal session (ended with Enter, `\r`, which a TUI in
/// raw mode and a line-mode program both take as the end of a line), or a
/// prompt for a structured one.
async fn send_text(
    target: &Target,
    session: &SessionSelector,
    text: String,
) -> Result<(), CliError> {
    let client = connect(target).await?.client;
    let info = session.resolve(&client).await?;
    let input = match info.spec.mode {
        SessionMode::Terminal { .. } => SessionInput::Terminal {
            input: TerminalInput::Data {
                data: TerminalBytes::new(Bytes::from(format!("{text}\r"))),
            },
        },
        SessionMode::Structured { .. } => SessionInput::Prompt {
            content: vec![ContentBlock::Text { text }],
        },
    };
    Ok(client.send(info.id, input).await?)
}

/// Print a session's log (see `log` for the order of stdout and stderr).
async fn log(target: &Target, session: &SessionSelector, from: EventSeq) -> Result<(), CliError> {
    let client = connect(target).await?.client;
    let info = session.resolve(&client).await?;
    crate::log::print(&client, info.id, from).await
}

#[cfg(test)]
mod tests {
    use vessel_client::book::Origin;

    use super::*;

    fn resolved(node: &str) -> Resolved {
        Resolved {
            name: None,
            address: node.parse().unwrap(),
            token: None,
            credential: None,
            pinned: None,
            origin: Origin::Named,
        }
    }

    #[test]
    fn a_relative_cwd_is_resolved_here_only_for_a_node_on_this_machine() {
        let here = std::env::current_dir().unwrap();
        let local = node_cwd(&resolved("127.0.0.1:7077"), PathBuf::from("project")).unwrap();
        assert_eq!(local, here.join("project"));
        let absolute = here.join("elsewhere");
        assert_eq!(
            node_cwd(&resolved("gpu-box"), absolute.clone()).unwrap(),
            absolute
        );
        for remote in ["gpu-box", "me@127.0.0.1", "ssh://localhost"] {
            assert!(
                matches!(
                    node_cwd(&resolved(remote), PathBuf::from("project")),
                    Err(CliError::RelativeCwd { .. })
                ),
                "{remote}"
            );
        }
    }
}
