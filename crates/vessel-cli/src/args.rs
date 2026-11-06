//! The command line, parsed once into typed values.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use vessel_client::book::NodeName;
use vessel_client::{BearerToken, RemotePath};
use vessel_proto::{EventSeq, HarnessKind};

use crate::selector::SessionSelector;

/// Vessel: drive agent harnesses on this and other machines.
#[derive(Debug, Parser)]
#[command(name = "vessel", version, about)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) target: Target,
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Which node to talk to, and with what credentials.
#[derive(Debug, Args)]
pub(crate) struct Target {
    /// Node to talk to: a name from your node book (`vessel nodes`), an
    /// SSH node (`ssh://[user@]host[:port]` or `user@host`), or a websocket
    /// node (`host`, `host:port`, a `ws://` / `http://` URL). Without it:
    /// the project's or the book's `default_node`, else the node on this
    /// machine (127.0.0.1:7077).
    #[arg(long, global = true, env = "VESSEL_NODE")]
    pub(crate) node: Option<String>,
    /// A websocket node's access token. For a node on this machine (a
    /// loopback address) it defaults to the token file of the local data
    /// directory; a book entry may name a token file; any other node needs
    /// it given explicitly. SSH nodes need none.
    #[arg(long, global = true, env = "VESSEL_TOKEN", hide_env_values = true)]
    pub(crate) token: Option<BearerToken>,
    /// Local data directory (default `~/.vessel`): where the token is read
    /// from, and where `node start` keeps its state.
    #[arg(long, global = true, env = "VESSEL_DATA_DIR")]
    pub(crate) data_dir: Option<PathBuf>,
    /// Configuration directory holding `vessel/nodes.toml` (default: the
    /// platform's, e.g. `~/.config`).
    #[arg(long, global = true, env = "VESSEL_CONFIG_DIR", hide = true)]
    pub(crate) config_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run a node in the foreground, or install one on another machine.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Your node book: named nodes for `--node`.
    Nodes {
        #[command(subcommand)]
        command: NodesCommand,
    },
    /// Show the node and the harnesses it can run.
    Status,
    /// List the harnesses installed on the node.
    Harnesses,
    /// List sessions, newest first.
    Sessions,
    /// Start a session and attach to it.
    Run(Run),
    /// Attach to a session: its terminal, or its conversation.
    Attach {
        /// Session id, or a unique prefix of one.
        session: SessionSelector,
        /// Replay from this record (0 replays everything).
        #[arg(long, default_value = "0", value_parser = parse_seq)]
        from: EventSeq,
    },
    /// Send a line (terminal session) or a prompt (structured session).
    Send {
        /// Session id, or a unique prefix of one.
        session: SessionSelector,
        /// What to send.
        text: String,
    },
    /// Print a session's log.
    Log {
        /// Session id, or a unique prefix of one.
        session: SessionSelector,
        /// First record to print.
        #[arg(long, default_value = "0", value_parser = parse_seq)]
        from: EventSeq,
    },
    /// Stop a session (its whole process group).
    Kill {
        /// Session id, or a unique prefix of one.
        session: SessionSelector,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum NodeCommand {
    /// Start a node and serve until Ctrl-C.
    Start {
        /// Address to listen on.
        #[arg(long, env = "VESSEL_BIND", default_value = "127.0.0.1:7077")]
        bind: SocketAddr,
        /// Node name shown to clients (defaults to the hostname).
        #[arg(long, env = "VESSEL_NAME")]
        name: Option<String>,
    },
    /// Install this client's vesseld on an SSH node (`--node`, or the
    /// default node). Nothing is restarted: a running node keeps its
    /// version until `vessel node update`.
    Install(Install),
    /// Install this client's vesseld on an SSH node and restart the node
    /// into it, as its live sessions allow.
    Update(Update),
    /// Show the node: version, state (serving or draining), live sessions.
    Status,
}

/// `vessel node update`.
#[derive(Debug, Args)]
pub(crate) struct Update {
    /// With live sessions: stop once the last one has ended (the node
    /// refuses new sessions meanwhile).
    #[arg(long, conflicts_with = "now")]
    pub(crate) when_idle: bool,
    /// With live sessions: stop now; they end interrupted (asks first).
    #[arg(long)]
    pub(crate) now: bool,
    /// Do not ask before `--now` interrupts sessions.
    #[arg(long, requires = "now")]
    pub(crate) yes: bool,
    /// Where vesseld comes from (see `vessel node install`).
    #[arg(long, value_enum, default_value_t = Source::Auto)]
    pub(crate) from: Source,
    /// The local vesseld build to upload (implies `--from local`).
    #[arg(long)]
    pub(crate) vesseld: Option<PathBuf>,
}

/// `vessel node install`.
#[derive(Debug, Args)]
pub(crate) struct Install {
    /// Where vesseld comes from: `local` uploads a vesseld built here (the
    /// one next to `vessel`, or `--vesseld`), `release` this version's
    /// release (downloaded with `gh`), `auto` a local build if there is one
    /// for that machine, else the release.
    #[arg(long, value_enum, default_value_t = Source::Auto)]
    pub(crate) from: Source,
    /// The local vesseld build to upload (implies `--from local`).
    #[arg(long)]
    pub(crate) vesseld: Option<PathBuf>,
}

/// Where an installed vesseld comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Source {
    Auto,
    Local,
    Release,
}

#[derive(Debug, Subcommand)]
pub(crate) enum NodesCommand {
    /// Add a node: `vessel nodes add gpu-box me@gpu-box`, or a websocket
    /// node with `--token-file`.
    Add {
        /// Its name (letters, digits, `.`, `_`, `-`).
        name: NodeName,
        /// How to reach it: `[user@]host` or `ssh://[user@]host[:port]` over
        /// SSH, or a `ws://` URL.
        address: String,
        /// A websocket node's token file.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// An SSH node's data directory on that machine (default
        /// `~/.vessel`).
        #[arg(long)]
        remote_data_dir: Option<RemotePath>,
        /// Make it the default node.
        #[arg(long)]
        default: bool,
    },
    /// List the nodes in the book.
    List,
    /// Show one node.
    Show {
        /// Its name.
        name: String,
    },
    /// Remove a node from the book.
    Remove {
        /// Its name.
        name: String,
    },
    /// Set (or with `--none`, clear) the default node.
    Default {
        /// The node.
        #[arg(required_unless_present = "none")]
        name: Option<String>,
        /// No default: commands then use the node on this machine.
        #[arg(long, conflicts_with = "name")]
        none: bool,
    },
}

/// `vessel run`.
#[derive(Debug, Args)]
pub(crate) struct Run {
    /// Harness to run: claude-code (or claude), codex, gemini, opencode.
    pub(crate) harness: HarnessKind,
    /// Drive it over ACP and show a conversation instead of its terminal UI.
    #[arg(long)]
    pub(crate) structured: bool,
    /// Model to request (structured mode).
    #[arg(long, requires = "structured")]
    pub(crate) model: Option<String>,
    /// Resume this harness session (structured mode).
    #[arg(long, requires = "structured")]
    pub(crate) resume: Option<String>,
    /// Working directory on the node (defaults to the node user's home). A
    /// relative path is taken from the current directory when the node is
    /// on this machine, and refused for a remote node.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
    /// Start it and print its id instead of attaching.
    #[arg(long)]
    pub(crate) detach: bool,
    /// Arguments passed to the harness verbatim (terminal mode), after `--`.
    #[arg(last = true, conflicts_with = "structured")]
    pub(crate) args: Vec<String>,
}

fn parse_seq(text: &str) -> Result<EventSeq, std::num::ParseIntError> {
    text.parse().map(EventSeq::new)
}
