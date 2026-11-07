//! Deterministic, offline LSP peer. Never starts a real language toolchain.

#[path = "fake_lsp/messages.rs"]
mod messages;
#[path = "fake_lsp/transport.rs"]
mod transport;
#[path = "fake_lsp/types.rs"]
mod types;

use std::io::{BufRead, Write};
use std::process::ExitCode;

use types::{Message, Mode};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn run() -> Result<(), transport::TransportError> {
    let mode = match std::env::args().nth(1).as_deref() {
        Some("content-modified") => Mode::ContentModified,
        Some("guard-go") => Mode::GuardGo,
        Some("guard-cpp") => Mode::GuardCpp,
        Some("hang") => Mode::Hang,
        Some("crash") => Mode::Crash,
        Some("incremental") => Mode::Incremental,
        Some("outside") => Mode::Outside,
        Some("silent") => Mode::Silent,
        Some("oversized") => Mode::Oversized,
        Some("push") => Mode::Push,
        Some("normal") | None => Mode::Normal,
        Some(_) => return Err(transport::TransportError::Protocol),
    };
    let arguments: Vec<_> = std::env::args_os().collect();
    if let Some(index) = arguments.iter().position(|argument| argument == "--record")
        && let Some(path) = arguments.get(index.saturating_add(1))
    {
        std::fs::write(path, "started")?;
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(&mut stdin.lock(), &mut stdout.lock(), mode)
}

fn serve(
    input: &mut impl BufRead,
    output: &mut impl Write,
    mode: Mode,
) -> Result<(), transport::TransportError> {
    let mut peer = messages::Peer::new(mode);
    while let Some(message) = transport::read::<Message>(input)? {
        if peer.handle(message, output)? {
            break;
        }
    }
    Ok(())
}
