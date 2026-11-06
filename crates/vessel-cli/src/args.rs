//! The command line, parsed once into typed values.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use vessel_client::{BearerToken, NodeUrl};
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
    /// Node to talk to: `host`, `host:port`, or a `ws://` / `http://` URL.
    #[arg(
        long,
        global = true,
        env = "VESSEL_NODE",
        default_value = "127.0.0.1:7077"
    )]
    pub(crate) node: NodeUrl,
    /// The node's access token. For a node on this machine (a loopback
    /// `--node`) it defaults to the token file of the local data directory;
    /// a remote node needs it given explicitly.
    #[arg(long, global = true, env = "VESSEL_TOKEN", hide_env_values = true)]
    pub(crate) token: Option<BearerToken>,
    /// Local data directory (default `~/.vessel`): where the token is read
    /// from, and where `node start` keeps its state.
    #[arg(long, global = true, env = "VESSEL_DATA_DIR")]
    pub(crate) data_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run a node in the foreground.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
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
