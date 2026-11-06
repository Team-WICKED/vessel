//! `vesseld`: the standalone node binary.
//!
//! - `vesseld [--bind ADDR|none]`: run the node until `Ctrl-C`, `SIGTERM`
//!   or a client's `node.stop`, then stop cleanly (sessions end as
//!   `Interrupted`, logs committed).
//! - `vesseld attach`: the proxy SSH runs (ADR 0007); stdout is the
//!   protocol stream, so it logs only warnings, to stderr.
//! - `vesseld version`: the version and target the installer checks.
//! - `vesseld cleanup`: delete installed binaries nothing needs (ADR 0008).
//! - `vesseld relay --bind ADDR --token-file PATH`: run a relay (ADR 0009)
//!   until `Ctrl-C` or `SIGTERM`; no data directory, nothing stored.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use vessel_harness::HarnessEnv;
use vessel_node::{Config, DataDir, Node, VesselMcp};
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
    /// Run a relay that nodes register with and clients reach them through.
    /// It speaks plain HTTP: put an HTTPS reverse proxy in front for
    /// `wss://`.
    // Boxed: the relay's flags outgrow every other command's (clippy's
    // `large_enum_variant`, which Windows' larger `PathBuf` tips over).
    Relay(Box<RelayArgs>),
}

/// `vesseld relay`.
#[derive(Debug, clap::Args)]
struct RelayArgs {
    /// Address to listen on.
    #[arg(long, env = "VESSEL_RELAY_BIND", default_value = "127.0.0.1:7078")]
    bind: SocketAddr,
    /// File holding the registration token nodes must present.
    #[arg(long, env = "VESSEL_RELAY_TOKEN_FILE")]
    token_file: PathBuf,
    /// Header in which the reverse proxy passes the client's address
    /// (`X-Real-IP`, `CF-Connecting-IP`), for per-client limits. Believed
    /// only from --trusted-proxy, which it needs; the proxy must overwrite
    /// it.
    #[arg(long, env = "VESSEL_RELAY_CLIENT_ADDRESS_HEADER")]
    client_address_header: Option<axum::http::HeaderName>,
    /// A reverse proxy whose --client-address-header is believed: an
    /// address or a network in CIDR form (`127.0.0.1`, `10.0.0.0/8`).
    /// Repeatable; comma-separated in the environment. Anyone else is
    /// counted by their own address.
    #[arg(
        long = "trusted-proxy",
        env = "VESSEL_RELAY_TRUSTED_PROXY",
        value_delimiter = ','
    )]
    trusted_proxies: Vec<vessel_relay::TrustedProxy>,
    /// Nodes registered at once; another is refused until one leaves.
    #[arg(long, env = "VESSEL_RELAY_MAX_NODES", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_nodes)]
    max_nodes: usize,
    /// Connections from nodes that are still registering, at once.
    #[arg(long, env = "VESSEL_RELAY_MAX_REGISTERING", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_registering)]
    max_registering: usize,
    /// Relayed client connections at once, in all. Each holds two sockets
    /// here: keep `max-nodes + 2 x max-clients` under the open-file limit.
    #[arg(long, env = "VESSEL_RELAY_MAX_CLIENTS", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_clients)]
    max_clients: usize,
    /// Relayed client connections to one node at once.
    #[arg(long, env = "VESSEL_RELAY_MAX_CLIENTS_PER_NODE", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_clients_per_node)]
    max_clients_per_node: usize,
    /// Bytes a second through each direction of one relayed connection,
    /// after a burst of one second's worth; 0 for no limit. At least
    /// 64 KiB (one record): a smaller value is raised to it.
    #[arg(long, env = "VESSEL_RELAY_MAX_BYTES_PER_SECOND",
        default_value_t = vessel_relay::Limits::default().max_bytes_per_second)]
    max_bytes_per_second: u64,
    /// Connections open at once, of every kind, counted from accept.
    #[arg(long, env = "VESSEL_RELAY_MAX_CONNECTIONS", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_connections)]
    max_connections: usize,
    /// Of --max-connections, the connections that have not authenticated
    /// yet (registered as a node, or paired as a client): the rest is kept
    /// for those that have. Less than half of --max-connections.
    #[arg(long, env = "VESSEL_RELAY_MAX_UNAUTHENTICATED", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_unauthenticated)]
    max_unauthenticated: usize,
    /// Connections not authenticated yet from one address (an IPv6 /64; a
    /// /56 twice this, a /48 four times), counted from accept; not
    /// applied to a trusted proxy's connections. Four times it must be
    /// less than half of --max-unauthenticated.
    #[arg(long, env = "VESSEL_RELAY_MAX_CONNECTIONS_PER_ADDRESS", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_connections_per_address)]
    max_connections_per_address: usize,
    /// Nodes registering at once from one address (a /48 four times this,
    /// which must be less than half of --max-registering).
    #[arg(long, env = "VESSEL_RELAY_MAX_REGISTERING_PER_ADDRESS", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_registering_per_address)]
    max_registering_per_address: usize,
    /// Of --max-registering, the places kept for nodes that registered
    /// here before (at most half).
    #[arg(long, env = "VESSEL_RELAY_RESERVED_REGISTERING",
        default_value_t = vessel_relay::Limits::default().reserved_registering)]
    reserved_registering: usize,
    /// Relayed client connections at once from one address (a /48 four
    /// times this, which must be less than half of --max-clients).
    #[arg(long, env = "VESSEL_RELAY_MAX_CLIENTS_PER_ADDRESS", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_clients_per_address)]
    max_clients_per_address: usize,
    /// Relayed client connections to one node at once from one address
    /// while the node admits them (their first seconds); a /48 four times
    /// this, which must be less than half of what a node admits at once
    /// through one relay (`vessel_relay::RELAYED_UNADMITTED`).
    #[arg(long, env = "VESSEL_RELAY_MAX_NODE_CLIENTS_PER_ADDRESS", value_parser = at_least_one,
        default_value_t = vessel_relay::Limits::default().max_node_clients_per_address)]
    max_node_clients_per_address: usize,
}

/// A count of at least one: a relay with no room for anything would only
/// refuse.
fn at_least_one(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(0) => Err("must be at least 1".to_owned()),
        Ok(count) => Ok(count),
        Err(error) => Err(error.to_string()),
    }
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

    // A relay keeps nothing on disk: it needs no data directory (nor a home).
    let command = match args.command {
        Some(Command::Relay(relay_args)) => return relay(relay_args).await,
        command => command,
    };
    let Some(root) = args.data_dir.or_else(DataDir::default_root) else {
        tracing::error!("no home directory; pass --data-dir");
        return ExitCode::FAILURE;
    };
    let dir = DataDir::new(root);
    match command {
        None => run(dir, args.run).await,
        Some(Command::Attach) => ExitCode::from(vessel_node::attach::run(dir).await.code()),
        Some(Command::Version) => print(&format!("{}\n", vessel_node::VERSION_LINE)),
        Some(Command::Cleanup { keep }) => cleanup(dir, keep).await,
        Some(Command::Relay(relay_args)) => relay(relay_args).await,
    }
}

async fn relay(args: Box<RelayArgs>) -> ExitCode {
    let args = *args;
    let token = match vessel_relay::read_token_file(&args.token_file) {
        Ok(token) => token,
        Err(error) => {
            tracing::error!(error = %chain(&error), "relay failed to start");
            return ExitCode::FAILURE;
        }
    };
    if args.client_address_header.is_some() && args.trusted_proxies.is_empty() {
        // A header believed from anyone lets anyone pick their address and
        // slip every per-address limit.
        tracing::error!(
            "relay failed to start: --client-address-header needs --trusted-proxy, the \
             proxies whose header is believed"
        );
        return ExitCode::FAILURE;
    }
    if args.client_address_header.is_none() && !args.trusted_proxies.is_empty() {
        tracing::warn!("--trusted-proxy does nothing without --client-address-header");
    }
    let mut config = vessel_relay::RelayConfig::new(token);
    config.client_address_header = args.client_address_header;
    config.trusted_proxies = args.trusted_proxies;
    let trusted_proxies = config
        .trusted_proxies
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let limits = &mut config.limits;
    limits.max_nodes = args.max_nodes;
    limits.max_registering = args.max_registering;
    limits.max_clients = args.max_clients;
    limits.max_clients_per_node = args.max_clients_per_node;
    limits.max_bytes_per_second = args.max_bytes_per_second;
    limits.max_connections = args.max_connections;
    limits.max_unauthenticated = args.max_unauthenticated;
    limits.max_connections_per_address = args.max_connections_per_address;
    limits.max_registering_per_address = args.max_registering_per_address;
    limits.reserved_registering = args.reserved_registering;
    limits.max_clients_per_address = args.max_clients_per_address;
    limits.max_node_clients_per_address = args.max_node_clients_per_address;
    if let Err(unbalanced) = limits.check() {
        // One /48 could hold half of a pool: the relay could be denied by
        // a single host's network.
        tracing::error!(error = %unbalanced, "relay failed to start");
        return ExitCode::FAILURE;
    }
    tracing::info!(
        max_nodes = limits.max_nodes,
        max_registering = limits.max_registering,
        max_clients = limits.max_clients,
        max_clients_per_node = limits.max_clients_per_node,
        max_bytes_per_second = limits.max_bytes_per_second,
        max_connections = limits.max_connections,
        max_unauthenticated = limits.max_unauthenticated,
        max_connections_per_address = limits.max_connections_per_address,
        max_registering_per_address = limits.max_registering_per_address,
        reserved_registering = limits.reserved_registering,
        max_clients_per_address = limits.max_clients_per_address,
        max_node_clients_per_address = limits.max_node_clients_per_address,
        %trusted_proxies,
        "relay limits"
    );
    let listener = match tokio::net::TcpListener::bind(args.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, address = %args.bind, "relay cannot listen");
            return ExitCode::FAILURE;
        }
    };
    if let Ok(address) = listener.local_addr() {
        tracing::info!(%address, "relay listening");
    }
    let shutdown = CancellationToken::new();
    let mut serving = tokio::spawn(vessel_relay::serve(listener, config, shutdown.clone()));
    let served = tokio::select! {
        () = vessel_node::signal() => {
            tracing::info!("stopping");
            shutdown.cancel();
            serving.await
        }
        served = &mut serving => served,
    };
    match served {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => {
            tracing::error!(%error, "relay failed");
            ExitCode::FAILURE
        }
        Err(error) => {
            tracing::error!(%error, "relay task failed");
            ExitCode::FAILURE
        }
    }
}

async fn run(dir: DataDir, args: Run) -> ExitCode {
    // The one place `vesseld` reads the account it runs as: the binary,
    // never the library (`HarnessEnv`).
    let Some(user_home) = dirs::home_dir() else {
        tracing::error!("this user has no home directory; set HOME");
        return ExitCode::FAILURE;
    };
    let env = HarnessEnv::from_vars(user_home, |name| std::env::var_os(name));
    let mut config = Config::new(dir, env);
    config.bind = match args.bind {
        Bind::Tcp(address) => Some(address),
        Bind::None => None,
    };
    config.name = args.name;
    config.agent_handshake_timeout = Duration::from_secs(args.agent_handshake_timeout);
    config.relays = args.relays;
    config.relay_token_file = args.relay_token_file;
    config.public = args.public;
    config.vessel_mcp = vessel_beside_me();

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

/// `vessel mcp` for sessions' agents (ADR 0013): the `vessel` installed
/// beside this `vesseld`, if there is one (releases ship both together);
/// otherwise sessions get no `vessel` server.
fn vessel_beside_me() -> Option<VesselMcp> {
    let me = std::env::current_exe().ok()?;
    let vessel = me.with_file_name(format!("vessel{}", std::env::consts::EXE_SUFFIX));
    vessel.is_file().then(|| VesselMcp::vessel(vessel))
}
