//! One function per command. Each connects, does its calls, and prints;
//! formatting lives in `render` and `view`.

use std::fmt::Write as _;
use std::net::SocketAddr;

use bytes::Bytes;
use vessel_client::NodeClient;
use vessel_node::{Config, Node};
use vessel_proto::{
    ContentBlock, EventSeq, HarnessSessionId, ModelId, SessionEvent, SessionInput, SessionMode,
    SessionSpec, TerminalBytes, TerminalInput,
};

use crate::args::{Cli, Command, NodeCommand, Run, Target};
use crate::attach::{self, terminal};
use crate::connect::{connect, data_dir};
use crate::error::CliError;
use crate::io;
use crate::render;
use crate::selector::SessionSelector;
use crate::view::Conversation;

/// Records asked for per page by `vessel log`.
const LOG_PAGE: u32 = 500;

pub(crate) async fn run(cli: Cli) -> Result<(), CliError> {
    let target = &cli.target;
    match cli.command {
        Command::Node {
            command: NodeCommand::Start { bind, name },
        } => node_start(target, bind, name).await,
        Command::Status => {
            let client = connect(target).await?;
            let node = client.node_info().await?;
            let harnesses = client.harnesses().await?;
            let text = format!("{}\n{}", render::node(&node), render::harnesses(&harnesses));
            Ok(io::out(text.as_bytes()).await?)
        }
        Command::Harnesses => {
            let client = connect(target).await?;
            let text = render::harnesses(&client.harnesses().await?);
            Ok(io::out(text.as_bytes()).await?)
        }
        Command::Sessions => {
            let client = connect(target).await?;
            let text = render::sessions(&client.sessions().await?);
            Ok(io::out(text.as_bytes()).await?)
        }
        Command::Run(run) => run_session(target, run).await,
        Command::Attach { session, from } => {
            let client = connect(target).await?;
            let info = session.resolve(&client).await?;
            attach::attach(&client, &info, from).await
        }
        Command::Send { session, text } => send_text(target, &session, text).await,
        Command::Log { session, from } => log(target, &session, from).await,
        Command::Kill { session } => {
            let client = connect(target).await?;
            let info = session.resolve(&client).await?;
            Ok(client.kill(info.id).await?)
        }
    }
}

async fn node_start(
    target: &Target,
    bind: SocketAddr,
    name: Option<String>,
) -> Result<(), CliError> {
    let mut config = Config::new(data_dir(target)?);
    config.bind = bind;
    config.name = name;
    let node = Node::start(config).await?;
    let started = format!(
        "[vessel] node {} listening on {}; Ctrl-C stops it\n",
        node.info().name,
        node.address()
    );
    io::note(&started).await?;
    vessel_node::signal().await;
    io::note("[vessel] stopping\n").await?;
    Ok(node.stop().await?)
}

async fn run_session(target: &Target, run: Run) -> Result<(), CliError> {
    let client = connect(target).await?;
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
    let spec = SessionSpec {
        harness: run.harness,
        cwd: run.cwd,
        mode,
    };
    let info = client.create_session(spec).await?;
    if run.detach {
        return Ok(io::out(format!("{}\n", info.id).as_bytes()).await?);
    }
    io::note(&format!("[vessel] session {}\n", info.id)).await?;
    attach::attach(&client, &info, EventSeq::ZERO).await
}

/// A line for a terminal session (ended with Enter, `\r`, which a TUI in
/// raw mode and a line-mode program both take as the end of a line), or a
/// prompt for a structured one.
async fn send_text(
    target: &Target,
    session: &SessionSelector,
    text: String,
) -> Result<(), CliError> {
    let client = connect(target).await?;
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

/// Print a session's log: terminal output as raw bytes, structured events as
/// text, status changes on stderr.
async fn log(target: &Target, session: &SessionSelector, from: EventSeq) -> Result<(), CliError> {
    let client = connect(target).await?;
    let info = session.resolve(&client).await?;
    print_log(&client, info.id, from).await
}

async fn print_log(
    client: &NodeClient,
    session: vessel_proto::SessionId,
    from: EventSeq,
) -> Result<(), CliError> {
    let mut conversation = Conversation::default();
    let mut next = from;
    loop {
        let page = client.read(session, next, LOG_PAGE).await?;
        let mut out = Vec::new();
        let mut notes = String::new();
        if let Some(first) = page.records.first()
            && first.seq > next
        {
            // Writing to a `String` cannot fail.
            let _infallible = writeln!(notes, "[vessel] records {next}..{} were pruned", first.seq);
        }
        for record in &page.records {
            match &record.event {
                SessionEvent::Terminal(output) => out.extend_from_slice(output.data.bytes()),
                SessionEvent::Status { .. } => notes.push_str(&conversation.show(record)),
                SessionEvent::Agent { .. } => {
                    out.extend_from_slice(conversation.show(record).as_bytes());
                }
            }
        }
        io::out(&out).await?;
        io::note(&notes).await?;
        next = page.next;
        if page.at_end {
            return Ok(());
        }
    }
}
