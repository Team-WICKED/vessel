//! `vesseld`: the standalone node binary.
//!
//! - `vesseld [--bind ADDR|none]`: run the node until `Ctrl-C`, `SIGTERM`
//!   or a client's `node.stop`, then stop cleanly (sessions end as
//!   `Interrupted`, logs committed).
//! - `vesseld attach`: the proxy SSH runs (ADR 0007); stdout is the
//!   protocol stream, so it logs only warnings, to stderr.
//! - `vesseld version`: the version and target the installer checks.
//! - `vesseld cleanup`: delete installed binaries nothing needs (ADR 0008).

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use clap::{Parser, Subcommand};
use vessel_node::{Config, DataDir, Node};
use vessel_proto::RouteUrl;

/// Vessel node daemon.
#[derive(Debug, Parser)]
#[command(
    name = "vesseld",
    version,
    about,
    args_conflicts_with_subcommands = true
)]
struct Args {
    /// Data directory (defaults to ~/.vessel).
    #[arg(long, global = true, env = "VESSEL_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(flatten)]
    run: Run,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Running the node (the default).
#[derive(Debug, clap::Args)]
struct Run {
    /// TCP address for the websocket, or `none` to serve only the local
    /// socket (reached through `vesseld attach`, for example over SSH).
    #[arg(long, env = "VESSEL_BIND", default_value = "127.0.0.1:7077")]
    bind: Bind,
    /// Node name shown to clients (defaults to the hostname).
    #[arg(long, env = "VESSEL_NAME")]
    name: Option<String>,
    /// Seconds a structured session's agent may take to start (the ACP
    /// handshake) before the session fails. Raise it where adapters start
    /// slowly, for example an `npx` adapter downloading on first use.
    #[arg(
        long,
        env = "VESSEL_AGENT_HANDSHAKE_TIMEOUT",
        value_name = "SECONDS",
        default_value_t = vessel_session::DEFAULT_HANDSHAKE_TIMEOUT.as_secs(),
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    agent_handshake_timeout: u64,
    /// A relay to register with, as a `ws://` or `wss://` URL (repeatable,
    /// tried in order). Pairing links name it, so devices reach this node
    /// through it without SSH or an open port.
    #[arg(long = "relay", value_name = "URL")]
    relays: Vec<RouteUrl>,
    /// File holding the relays' registration token (needed with
    /// `--relay`).
    #[arg(long, value_name = "PATH", env = "VESSEL_RELAY_TOKEN_FILE")]
    relay_token_file: Option<PathBuf>,
    /// An address devices may dial directly, advertised in pairing links
    /// (repeatable): `ws://HOST:PORT`, `wss://...`, or just `HOST:PORT`.
    /// Nothing is guessed: without it, devices go through a relay.
    #[arg(long = "public", value_name = "ADDR", value_parser = public_route)]
    public: Vec<RouteUrl>,
    /// Started in the background by `vesseld attach`: announce readiness
    /// on stdout and keep running when the SSH session hangs up.
    #[arg(long, hide = true)]
    detached: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Bridge stdin and stdout to this machine's node, starting it if none
    /// runs. This is what `vessel` runs over SSH.
    Attach,
    /// Print this binary's version and target triple.
    Version,
    /// Delete installed vesseld binaries that are no longer needed.
    Cleanup {
        /// Newest installed binaries to keep besides the running one.
        #[arg(long, default_value = "1")]
        keep: usize,
    },
}

/// `--bind`: an address, or `none`.
#[derive(Debug, Clone, Copy)]
enum Bind {
    Tcp(SocketAddr),
    None,
}

impl FromStr for Bind {
    type Err = std::net::AddrParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.eq_ignore_ascii_case("none") {
            Ok(Self::None)
        } else {
            text.parse().map(Self::Tcp)
        }
    }
}

/// `--public`: a websocket URL, or `HOST:PORT` taken as `ws://HOST:PORT`.
fn public_route(text: &str) -> Result<RouteUrl, String> {
    let text = text.trim();
    let url = if text.contains("://") {
        text.to_owned()
    } else {
        format!("ws://{text}")
    };
    url.parse()
        .map_err(|error: vessel_proto::InvalidRoute| error.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let attaching = matches!(args.command, Some(Command::Attach));
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_unset| if attaching { "warn" } else { "info" }.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let Some(root) = args.data_dir.or_else(DataDir::default_root) else {
        tracing::error!("no home directory; pass --data-dir");
        return ExitCode::FAILURE;
    };
    let dir = DataDir::new(root);
    match args.command {
        None => run(dir, args.run).await,
        Some(Command::Attach) => ExitCode::from(vessel_node::attach::run(dir).await.code()),
        Some(Command::Version) => print(&format!("{}\n", vessel_node::VERSION_LINE)),
        Some(Command::Cleanup { keep }) => cleanup(dir, keep).await,
    }
}

async fn run(dir: DataDir, args: Run) -> ExitCode {
    let mut config = Config::new(dir);
    config.bind = match args.bind {
        Bind::Tcp(address) => Some(address),
        Bind::None => None,
    };
    config.name = args.name;
    config.agent_handshake_timeout = Duration::from_secs(args.agent_handshake_timeout);
    config.relays = args.relays;
    config.relay_token_file = args.relay_token_file;
    config.public = args.public;

    // Before the node starts, so a hang-up during start-up cannot kill it.
    #[cfg(unix)]
    let _hangups = if args.detached {
        ignore_hangups()
    } else {
        None
    };
    let node = match Node::start(config).await {
        Ok(node) => node,
        Err(error) => {
            tracing::error!(error = %chain(&error), "node failed to start");
            return ExitCode::FAILURE;
        }
    };
    if args.detached {
        let _written = print(&format!(
            "{} {}\n",
            vessel_node::attach::READY,
            std::process::id()
        ));
    }
    tokio::select! {
        () = vessel_node::signal() => {}
        () = node.stop_requested() => {}
    }
    tracing::info!("stopping");
    match node.stop().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %chain(&error), "node failed");
            ExitCode::FAILURE
        }
    }
}

/// A node started by `attach` must survive the end of the SSH session that
/// started it. Listening for `SIGHUP` replaces its default action (exit)
/// for the rest of the process; the stream itself is never read.
#[cfg(unix)]
fn ignore_hangups() -> Option<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .inspect_err(|error| tracing::warn!(%error, "cannot ignore SIGHUP"))
        .ok()
}

async fn cleanup(dir: DataDir, keep: usize) -> ExitCode {
    let cleaned =
        tokio::task::spawn_blocking(move || vessel_node::cleanup::cleanup(&dir, keep)).await;
    match cleaned {
        Ok(Ok(deleted)) => {
            let mut text = String::new();
            for path in &deleted {
                text.push_str(&path.display().to_string());
                text.push('\n');
            }
            print(&text)
        }
        Ok(Err(error)) => {
            tracing::error!(error = %chain(&error), "cleanup failed");
            ExitCode::FAILURE
        }
        Err(error) => {
            tracing::error!(%error, "cleanup task failed");
            ExitCode::FAILURE
        }
    }
}

/// Write `text` to stdout and flush it.
fn print(text: &str) -> ExitCode {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "cannot write to stdout");
            ExitCode::FAILURE
        }
    }
}

/// The error and its causes on one line.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
