//! The command line, parsed once into typed values.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use vessel_client::book::NodeName;
use vessel_client::{BearerToken, RemotePath};
use vessel_node::fleet::SkipGlob;
use vessel_proto::{EventSeq, HarnessKind, ProfileName};

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
    /// List the node's harness profiles (`--profile` for `run`).
    Profiles {
        /// Only this harness's profiles.
        #[arg(long)]
        harness: Option<HarnessKind>,
        /// Have the node read its `profiles.toml` again first (needs the
        /// `admin` scope). A file that does not parse changes nothing.
        #[arg(long)]
        reload: bool,
    },
    /// Fleet sync: one node, the home, shares its instructions and skills;
    /// member nodes follow it into every profile (ADR 0012).
    Fleet {
        #[command(subcommand)]
        command: FleetCommand,
    },
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
    /// Set this machine up, or check it again: the node's identity, the
    /// harnesses (offering to install what is missing), profiles for the
    /// other accounts found, their logins, and with `--service` the node
    /// as a per-user service. Safe to run again: it fills gaps and changes
    /// nothing else.
    Init(InitArgs),
    /// Serve the Vessel tools over MCP on stdio, as a session's agent. A
    /// node starts this for every session (the session's `vessel` MCP
    /// server), with its endpoint and the session's credential in the
    /// environment; it is not run by hand.
    Mcp,
    /// Change a session in place.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

/// `vessel init`'s options.
#[derive(Debug, Args)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one command-line flag; `init::Options` folds them into a consent"
)]
pub(crate) struct InitArgs {
    /// Also install the node as a per-user service (systemd user unit on
    /// Linux, launchd agent on macOS) and start it.
    #[arg(long)]
    pub(crate) service: bool,
    /// Join a fleet: paste the link `vessel fleet invite` printed on the
    /// fleet's home (read from standard input, never the command line).
    #[arg(long)]
    pub(crate) join: bool,
    /// Answer yes to every question, except installs.
    #[arg(long, short = 'y')]
    pub(crate) yes: bool,
    /// With --yes, run the install commands for missing harnesses too.
    #[arg(long, requires = "yes")]
    pub(crate) install: bool,
    /// The home directory to look in for other accounts (tests).
    #[arg(long, env = "VESSEL_HOME", hide = true)]
    pub(crate) home: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SessionCommand {
    /// Continue a structured session's conversation under another profile
    /// (another account of the same harness), for example at a usage
    /// limit. The target profile must be logged in.
    MoveProfile {
        /// Session id, or a unique prefix of one.
        session: SessionSelector,
        /// The profile to move to (`vessel profiles`); the harness's name
        /// is its default profile.
        profile: ProfileName,
        /// Move even in the middle of a turn, cancelling it.
        #[arg(long)]
        force: bool,
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
    /// Make a single-use link that pairs a new device with the node (needs
    /// the `admin` scope). Prints the link, and a QR code on a terminal.
    Pair {
        /// Name for the device on the node (else it names itself).
        #[arg(long)]
        label: Option<String>,
        /// How long the link works: `90s`, `10m`, `2h` (at most `24h`).
        #[arg(long, default_value = "10m", value_parser = ttl_secs)]
        ttl: u32,
    },
    /// List the node's paired devices, or revoke one.
    Devices {
        #[command(subcommand)]
        command: Option<DevicesCommand>,
    },
}

/// `vessel fleet ...`. `init`, `join`, `leave`, `status` and `restore` act
/// on the node of this machine's data directory (`--data-dir`), running or
/// not; `invite` and `members` talk to a node (`--node`, the home).
#[derive(Debug, Subcommand)]
pub(crate) enum FleetCommand {
    /// Make this node the fleet's home: write `fleet.toml`, naming the
    /// instructions file and skills directory every member gets.
    Init {
        /// The instructions file (`~/.codex/AGENTS.md`, say).
        #[arg(long)]
        instructions: Option<PathBuf>,
        /// The skills directory (`~/.codex/skills`, say).
        #[arg(long)]
        skills: Option<PathBuf>,
        /// Names to leave out (`*` and `?` within one name), such as
        /// skills another tool manages; repeatable.
        #[arg(long = "skip", value_name = "GLOB")]
        skip: Vec<SkipGlob>,
    },
    /// Make a single-use link for a node to join this home (needs the
    /// `admin` scope on the home). Prints the link, and a QR code on a
    /// terminal.
    Invite {
        /// Name for the member on the home (else it names itself).
        #[arg(long)]
        label: Option<String>,
        /// How long the link works: `90s`, `10m`, `2h` (at most `24h`).
        #[arg(long, default_value = "10m", value_parser = ttl_secs)]
        ttl: u32,
    },
    /// Join the fleet whose home made the link: paste it at the prompt, or
    /// pipe it in with `--yes` (never on the command line).
    Join {
        /// Join without asking, after checking the key's fingerprint.
        #[arg(long)]
        yes: bool,
    },
    /// Stop following the home. The files it wrote stay as they are.
    Leave,
    /// This node's place in the fleet: its role, what it last applied, and
    /// the files it left alone and why.
    Status,
    /// The home's members and what each last applied (needs `admin`).
    Members,
    /// Bring back fleet files deleted here: one file, or every one below a
    /// directory. The node writes them again within a few seconds.
    Restore {
        /// The deleted file, or a directory.
        path: PathBuf,
    },
}

/// `vessel node devices ...`.
#[derive(Debug, Subcommand)]
pub(crate) enum DevicesCommand {
    /// Revoke a device: it is refused from now on and its open connections
    /// close. Other devices are not affected.
    Revoke {
        /// The device's id, or a unique prefix of it.
        device: String,
    },
}

/// `--pair` reads its link from standard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FromStdin;

/// `--pair` or `--pair -`, and nothing else: a link on the command line is
/// readable by every user of the machine (`ps`, `/proc`) and stays in the
/// shell's history, so someone else could redeem it first.
fn link_from_stdin(text: &str) -> Result<FromStdin, String> {
    if text == "-" {
        return Ok(FromStdin);
    }
    Err(
        "the pairing link is read from standard input, never from the command line, where \
         other users of this machine can read it and your shell history keeps it. Make a new \
         link with `vessel node pair` (this one has been exposed), then run `vessel nodes add \
         NAME --pair` and paste it"
            .to_owned(),
    )
}

/// Longest pairing link lifetime, as the node allows (ADR 0010).
const MAX_TTL_SECS: u32 = 24 * 60 * 60;

/// `--ttl`: a number of seconds, minutes (`m`) or hours (`h`), from one
/// second to a day.
fn ttl_secs(text: &str) -> Result<u32, String> {
    let text = text.trim();
    let (number, unit) = match text.strip_suffix(['s', 'm', 'h']) {
        Some(number) => (number, text.chars().last()),
        None => (text, None),
    };
    let scale = match unit {
        Some('m') => 60,
        Some('h') => 60 * 60,
        _ => 1,
    };
    let invalid = || format!("`{text}` is not a duration like `90s`, `10m` or `2h`");
    let secs = number
        .parse::<u32>()
        .map_err(|_number| invalid())?
        .checked_mul(scale)
        .ok_or_else(invalid)?;
    if secs == 0 {
        return Err("a pairing link must work for at least a second".to_owned());
    }
    if secs > MAX_TTL_SECS {
        return Err("a pairing link works at most 24h".to_owned());
    }
    Ok(secs)
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
    /// Add a node: `vessel nodes add gpu-box me@gpu-box`, a websocket node
    /// with `--token-file`, or pair with one: `vessel nodes add gpu-box
    /// --pair`, then paste the link `vessel node pair` printed.
    Add {
        /// Its name (letters, digits, `.`, `_`, `-`).
        name: NodeName,
        /// How to reach it: `[user@]host` or `ssh://[user@]host[:port]` over
        /// SSH, or a `ws://` URL.
        #[arg(required_unless_present = "pair")]
        address: Option<String>,
        /// A websocket node's token file.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// An SSH node's data directory on that machine (default
        /// `~/.vessel`).
        #[arg(long)]
        remote_data_dir: Option<RemotePath>,
        /// Pair with the node: the link is read from standard input (pasted
        /// at the prompt, or piped), never from the command line.
        #[arg(
            long,
            num_args = 0..=1,
            default_missing_value = "-",
            value_name = "-",
            value_parser = link_from_stdin,
            conflicts_with_all = ["address", "token_file", "remote_data_dir"],
        )]
        pair: Option<FromStdin>,
        /// Pair without asking to confirm the node's key (needed when the
        /// link is piped in). Only with `--pair`.
        #[arg(long)]
        yes: bool,
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
    /// Run under this profile of the node (`vessel profiles` lists them);
    /// the harness's default when absent.
    #[arg(long)]
    pub(crate) profile: Option<ProfileName>,
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

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    #[test]
    fn run_takes_a_profile_and_profiles_a_harness() {
        let Cli {
            command: Command::Run(run),
            ..
        } = Cli::try_parse_from(["vessel", "run", "claude", "--profile", "claude-alt"]).unwrap()
        else {
            panic!("expected run");
        };
        assert_eq!(run.profile.unwrap().as_str(), "claude-alt");
        assert!(Cli::try_parse_from(["vessel", "run", "claude", "--profile", "bad name"]).is_err());
        let Cli {
            command: Command::Profiles { harness, reload },
            ..
        } = Cli::try_parse_from(["vessel", "profiles", "--harness", "codex", "--reload"]).unwrap()
        else {
            panic!("expected profiles");
        };
        assert_eq!(harness, Some(HarnessKind::Codex));
        assert!(reload);
    }

    #[test]
    fn move_profile_takes_a_session_a_profile_and_force() {
        let Cli {
            command:
                Command::Session {
                    command: SessionCommand::MoveProfile { profile, force, .. },
                },
            ..
        } = Cli::try_parse_from(["vessel", "session", "move-profile", "3f2a", "claude-alt"])
            .unwrap()
        else {
            panic!("expected session move-profile");
        };
        assert_eq!(profile.as_str(), "claude-alt");
        assert!(!force);
        assert!(Cli::try_parse_from(["vessel", "session", "move-profile", "3f2a", "a b"]).is_err());
        assert!(Cli::try_parse_from(["vessel", "session", "move-profile", "3f2a"]).is_err());
        assert!(
            Cli::try_parse_from(["vessel", "session", "move-profile", "3f2a", "x", "--force"])
                .is_ok()
        );
    }

    /// Review item 3: a pairing link never comes from the command line,
    /// where other users and the shell's history see it.
    #[test]
    fn a_pairing_link_on_the_command_line_is_refused() {
        let parse = |args: &[&str]| Cli::try_parse_from(args);
        let error = parse(&[
            "vessel",
            "nodes",
            "add",
            "box",
            "--pair",
            "vessel://pair#e30",
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("standard input"), "{error}");
        assert!(error.contains("Make a new link"), "{error}");
        for args in [
            &["vessel", "nodes", "add", "box", "--pair"][..],
            &["vessel", "nodes", "add", "box", "--pair", "-"],
            &["vessel", "nodes", "add", "box", "--pair", "--yes"],
        ] {
            let Cli {
                command:
                    Command::Nodes {
                        command: NodesCommand::Add { pair, address, .. },
                    },
                ..
            } = parse(args).unwrap()
            else {
                panic!("{args:?}");
            };
            assert_eq!((pair, address), (Some(FromStdin), None), "{args:?}");
        }
        assert!(parse(&["vessel", "nodes", "add", "box", "me@box", "--pair"]).is_err());
    }

    #[test]
    fn pairing_link_lifetimes_read_like_durations_and_stay_within_a_day() {
        assert_eq!(ttl_secs("90"), Ok(90));
        assert_eq!(ttl_secs("90s"), Ok(90));
        assert_eq!(ttl_secs("10m"), Ok(600));
        assert_eq!(ttl_secs(" 2h "), Ok(7200));
        assert_eq!(ttl_secs("24h"), Ok(MAX_TTL_SECS));
        assert!(ttl_secs("25h").is_err());
        assert!(ttl_secs("0m").is_err());
        assert!(ttl_secs("ten").is_err());
        assert!(ttl_secs("1d").is_err());
        assert!(ttl_secs("4294967295h").is_err(), "overflow is an error");
    }
}
