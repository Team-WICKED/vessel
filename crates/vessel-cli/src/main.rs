//! `vessel`: command-line client for a Vessel node.

use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use futures_util::StreamExt as _;
use tokio_tungstenite::tungstenite::Message;
use vessel_core::{
    Harness, HarnessKind, NodeInfo, SessionEvent, SessionId, SessionInfo, SessionInput,
    SessionSpec, SessionStatus, StreamKind,
};
use vessel_node::Config;

/// Vessel: drive agent harnesses on this and other machines.
#[derive(Debug, Parser)]
#[command(name = "vessel", version, about)]
struct Cli {
    /// Base URL of the node to talk to.
    #[arg(
        long,
        env = "VESSEL_NODE",
        default_value = "http://127.0.0.1:7077",
        global = true
    )]
    node: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a node in the foreground.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Show the node and the harnesses it can run.
    Status,
    /// List harnesses installed on the node.
    Harnesses,
    /// List sessions on the node.
    Sessions,
    /// Start a harness session and attach to its output.
    Run {
        /// Harness to run (claude-code, codex, gemini, opencode).
        harness: HarnessKind,
        /// Working directory on the node.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Start it and return immediately instead of attaching.
        #[arg(long)]
        detach: bool,
        /// Arguments passed to the harness verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Stream a session's output.
    Attach {
        /// Session id.
        id: SessionId,
    },
    /// Write a line to a session's stdin.
    Send {
        /// Session id.
        id: SessionId,
        /// Text to send. A newline is appended.
        text: String,
    },
    /// Kill a session.
    Kill {
        /// Session id.
        id: SessionId,
    },
}

#[derive(Debug, Subcommand)]
enum NodeCommand {
    /// Start the node.
    Start {
        /// Address to listen on.
        #[arg(long, env = "VESSEL_BIND", default_value_t = Config::default().bind)]
        bind: SocketAddr,
        /// Node name shown to clients.
        #[arg(long, env = "VESSEL_NAME")]
        name: Option<String>,
        /// Data directory.
        #[arg(long, env = "VESSEL_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Node(#[from] vessel_node::NodeError),
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("node answered {status}: {message}")]
    Api {
        status: reqwest::StatusCode,
        message: String,
    },
    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("bad event from node: {0}")]
    Decode(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

type Result<T> = std::result::Result<T, CliError>;

struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn ws_url(&self, path: &str) -> String {
        let base = self
            .base
            .strip_prefix("https://")
            .map(|rest| format!("wss://{rest}"))
            .or_else(|| {
                self.base
                    .strip_prefix("http://")
                    .map(|rest| format!("ws://{rest}"))
            })
            .unwrap_or_else(|| self.base.clone());
        format!("{base}{path}")
    }

    async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let message = response
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        Err(CliError::Api { status, message })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.http.get(self.url(path)).send().await?;
        Ok(Self::check(response).await?.json().await?)
    }

    async fn post<B: serde::Serialize>(&self, path: &str, body: &B) -> Result<reqwest::Response> {
        let response = self.http.post(self.url(path)).json(body).send().await?;
        Self::check(response).await
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let response = self.http.delete(self.url(path)).send().await?;
        drop(Self::check(response).await?);
        Ok(())
    }
}

fn print_node(out: &mut impl io::Write, node: &NodeInfo) -> io::Result<()> {
    writeln!(out, "node      {} ({})", node.name, node.id)?;
    writeln!(out, "host      {} {}/{}", node.hostname, node.os, node.arch)?;
    writeln!(
        out,
        "version   {} (protocol {})",
        node.version, node.protocol
    )?;
    writeln!(out, "started   {}", node.started_at.to_rfc3339())
}

fn print_harnesses(out: &mut impl io::Write, harnesses: &[Harness]) -> io::Result<()> {
    if harnesses.is_empty() {
        return writeln!(out, "no harnesses found on PATH");
    }
    for h in harnesses {
        let version = h.version.as_deref().unwrap_or("-");
        writeln!(out, "{:<12} {:<40} {}", h.kind, version, h.path.display())?;
    }
    Ok(())
}

fn describe_status(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Starting => "starting".to_owned(),
        SessionStatus::Running => "running".to_owned(),
        SessionStatus::Exited { code: Some(code) } => format!("exited({code})"),
        SessionStatus::Exited { code: None } => "exited".to_owned(),
        SessionStatus::Failed { reason } => format!("failed: {reason}"),
    }
}

fn print_sessions(out: &mut impl io::Write, sessions: &[SessionInfo]) -> io::Result<()> {
    if sessions.is_empty() {
        return writeln!(out, "no sessions");
    }
    for s in sessions {
        writeln!(
            out,
            "{}  {:<12} {:<14} {}",
            s.id,
            s.spec.harness,
            describe_status(&s.status),
            s.spec.args.join(" ")
        )?;
    }
    Ok(())
}

async fn attach(client: &Client, id: SessionId) -> Result<()> {
    let url = client.ws_url(&format!("/sessions/{id}/stream"));
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await?;
    let mut out = io::stdout().lock();
    let mut err = io::stderr().lock();
    while let Some(message) = socket.next().await {
        let text = match message? {
            Message::Text(text) => text,
            Message::Close(_) => break,
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                continue;
            }
        };
        match serde_json::from_str::<SessionEvent>(&text)? {
            SessionEvent::Output(chunk) => match chunk.stream {
                StreamKind::Stdout => writeln!(out, "{}", chunk.line)?,
                StreamKind::Stderr => writeln!(err, "{}", chunk.line)?,
            },
            SessionEvent::Status { status } => {
                writeln!(err, "[vessel] session {}", describe_status(&status))?;
                if !status.is_live() {
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let mut out = io::stdout().lock();
    let client = Client::new(&cli.node);
    match cli.command {
        Command::Node {
            command:
                NodeCommand::Start {
                    bind,
                    name,
                    data_dir,
                },
        } => {
            vessel_node::serve(Config {
                bind,
                name,
                data_dir,
            })
            .await?;
        }
        Command::Status => {
            let node: NodeInfo = client.get("/node").await?;
            let harnesses: Vec<Harness> = client.get("/harnesses").await?;
            print_node(&mut out, &node)?;
            writeln!(out)?;
            print_harnesses(&mut out, &harnesses)?;
        }
        Command::Harnesses => {
            let harnesses: Vec<Harness> = client.get("/harnesses").await?;
            print_harnesses(&mut out, &harnesses)?;
        }
        Command::Sessions => {
            let sessions: Vec<SessionInfo> = client.get("/sessions").await?;
            print_sessions(&mut out, &sessions)?;
        }
        Command::Run {
            harness,
            cwd,
            detach,
            args,
        } => {
            let spec = SessionSpec { harness, cwd, args };
            let info: SessionInfo = client.post("/sessions", &spec).await?.json().await?;
            writeln!(out, "{}", info.id)?;
            if !detach {
                drop(out);
                attach(&client, info.id).await?;
            }
        }
        Command::Attach { id } => {
            drop(out);
            attach(&client, id).await?;
        }
        Command::Send { id, text } => {
            let body = SessionInput {
                text: format!("{text}\n"),
            };
            drop(client.post(&format!("/sessions/{id}/input"), &body).await?);
        }
        Command::Kill { id } => {
            client.delete(&format!("/sessions/{id}")).await?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let mut stderr = io::stderr().lock();
            drop(writeln!(stderr, "error: {err}"));
            ExitCode::FAILURE
        }
    }
}
