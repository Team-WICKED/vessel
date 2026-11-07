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
        Some("python-config") => Mode::PythonConfig,
        Some("normal") | None => Mode::Normal,
        Some(_) => return Err(transport::TransportError::Protocol),
    };
    let arguments: Vec<_> = std::env::args_os().collect();
    let option = |name: &str| {
        arguments
            .iter()
            .position(|argument| argument == name)
            .and_then(|index| arguments.get(index.saturating_add(1)))
            .map(std::path::PathBuf::from)
    };
    if let Some(path) = option("--record") {
        std::fs::write(path, "started")?;
    }
    // Tests read back what the launcher actually gave this process: its
    // working directory, the toolchain variables and (later) the raw
    // initialization options, so a launch policy is checked end to end.
    let launch = option("--record-launch");
    if let Some(path) = &launch {
        let mut record = std::fs::File::create(path)?;
        writeln!(record, "cwd={}", std::env::current_dir()?.display())?;
        for name in [
            "RUSTC",
            "CARGO",
            "RUSTUP_TOOLCHAIN",
            "TMPDIR",
            "TEMP",
            "HOME",
        ] {
            if let Some(value) = std::env::var_os(name) {
                writeln!(record, "{name}={}", value.to_string_lossy())?;
            }
        }
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(&mut stdin.lock(), &mut stdout.lock(), mode, launch)
}

fn serve(
    input: &mut impl BufRead,
    output: &mut impl Write,
    mode: Mode,
    launch: Option<std::path::PathBuf>,
) -> Result<(), transport::TransportError> {
    let mut peer = messages::Peer::new(mode, launch);
    while let Some(message) = transport::read::<Message>(input)? {
        if peer.handle(message, output)? {
            break;
        }
    }
    Ok(())
}
