//! `fake-tui`: a stand-in for a terminal harness.
//!
//! Behaviour, in order:
//!
//! 1. `fake-tui --version` prints `fake-tui <version>` and exits 0, like the
//!    version probe expects of a real harness.
//! 2. Otherwise it starts a grandchild (`fake-tui --sleeper`) that inherits
//!    stdout and sleeps forever, and prints `grandchild <pid>`. The
//!    grandchild stays in the process group, so it dies only if the group is
//!    killed, and while it lives the output stream (pipe or PTY) never ends.
//!    Tests use "output reached EOF" as proof the whole group is gone.
//! 3. On Unix, if stdout is a terminal, it prints `size <cols>x<rows>` now
//!    and again on every `SIGWINCH`.
//! 4. It prints `ready` and a `> ` prompt, then handles stdin lines:
//!    `exit [code]` exits with that code (default 0), `env NAME` prints
//!    `env NAME=<value>` or `env NAME unset`, `cwd` prints `cwd <dir>`,
//!    `modes on` / `modes off` switch the terminal modes a full-screen TUI
//!    sets (alternate screen, hidden cursor, bracketed paste, kitty
//!    keyboard flags, as Codex's TUI does) and print `modes on` / `modes
//!    off`, `escape` (Unix) starts a sleeper in a process group of its own,
//!    which survives the group kill and keeps the output open, and prints
//!    `escaped <pid>`, and anything else prints `echo: <line>`. EOF on stdin
//!    exits 0.

use std::io::{self, BufRead as _, Write as _};
use std::process::{ExitCode, Stdio};

use tokio::sync::mpsc;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--version") => {
            say(&format!("fake-tui {}", env!("CARGO_PKG_VERSION")));
            ExitCode::SUCCESS
        }
        Some("--sleeper") => loop {
            // Parked forever; `park` may return spuriously, hence the loop.
            std::thread::park();
        },
        _ => run(),
    }
}

fn run() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            say(&format!("error: no runtime: {error}"));
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(session())
}

async fn session() -> ExitCode {
    match spawn_sleeper() {
        Ok(pid) => say(&format!("grandchild {pid}")),
        Err(error) => say(&format!("error: no grandchild: {error}")),
    }
    let mut resizes = match Resizes::new() {
        Ok(resizes) => resizes,
        Err(error) => {
            say(&format!("error: no resize signal: {error}"));
            return ExitCode::FAILURE;
        }
    };
    report_size();
    say("ready");
    prompt();

    let mut lines = stdin_lines();
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else {
                    return ExitCode::SUCCESS;
                };
                if let Some(code) = handle(line.trim_end()) {
                    return code;
                }
                prompt();
            }
            () = resizes.next() => report_size(),
        }
    }
}

/// Run one command line; `Some` means exit with that code.
fn handle(line: &str) -> Option<ExitCode> {
    if let Some(rest) = line.strip_prefix("exit") {
        let code = rest.trim().parse::<u8>().unwrap_or(0);
        return Some(ExitCode::from(code));
    }
    if let Some(name) = line.strip_prefix("env ") {
        match std::env::var_os(name) {
            Some(value) => say(&format!("env {name}={}", value.to_string_lossy())),
            None => say(&format!("env {name} unset")),
        }
    } else if line == "modes on" {
        // Alternate screen, hide the cursor, bracketed paste, push kitty
        // keyboard flags (disambiguate escape codes).
        raw(b"\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[>1u");
        say("modes on");
    } else if line == "modes off" {
        raw(b"\x1b[<u\x1b[?2004l\x1b[?25h\x1b[?1049l");
        say("modes off");
    } else if line == "escape" {
        match spawn_escapee() {
            Ok(pid) => say(&format!("escaped {pid}")),
            Err(error) => say(&format!("error: no escape: {error}")),
        }
    } else if line == "cwd" {
        match std::env::current_dir() {
            Ok(dir) => say(&format!("cwd {}", dir.display())),
            Err(error) => say(&format!("error: no cwd: {error}")),
        }
    } else {
        say(&format!("echo: {line}"));
    }
    None
}

/// Start the sleeping grandchild; returns its pid.
fn spawn_sleeper() -> io::Result<u32> {
    let child = std::process::Command::new(std::env::current_exe()?)
        .arg("--sleeper")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    // Dropping a std `Child` neither waits for nor kills it; the grandchild
    // is meant to outlive this handle.
    Ok(child.id())
}

/// Start a sleeper in a process group of its own (Unix): killing this
/// program's group does not reach it, and it keeps the output open, like a
/// daemon a harness started and forgot. Returns its pid.
#[cfg(unix)]
fn spawn_escapee() -> io::Result<u32> {
    use std::os::unix::process::CommandExt as _;

    let child = std::process::Command::new(std::env::current_exe()?)
        .arg("--sleeper")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()?;
    Ok(child.id())
}

#[cfg(not(unix))]
fn spawn_escapee() -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process groups are Unix-only",
    ))
}

/// Write bytes as they are (escape sequences), flushed.
fn raw(bytes: &[u8]) {
    let mut out = io::stdout().lock();
    let written = out.write_all(bytes).and_then(|()| out.flush());
    drop(written);
}

/// Stdin lines, read on a plain thread.
///
/// Not `tokio::io::stdin`: that reads on the runtime's blocking pool, and a
/// runtime does not shut down while a blocking read is pending, so `exit`
/// would hang. A detached thread simply ends with the process.
fn stdin_lines() -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel(16);
    drop(std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if sender.blocking_send(line).is_err() {
                break;
            }
        }
    }));
    receiver
}

fn say(line: &str) {
    let mut out = io::stdout().lock();
    let written = writeln!(out, "{line}").and_then(|()| out.flush());
    // A closed stdout means the reader went away; there is nobody left to
    // report the failure to.
    drop(written);
}

fn prompt() {
    let mut out = io::stdout().lock();
    let written = write!(out, "> ").and_then(|()| out.flush());
    drop(written);
}

#[cfg(unix)]
fn report_size() {
    if let Ok(size) = rustix::termios::tcgetwinsize(io::stdout()) {
        say(&format!("size {}x{}", size.ws_col, size.ws_row));
    }
}

#[cfg(not(unix))]
fn report_size() {}

/// Window-change notifications.
#[cfg(unix)]
struct Resizes(tokio::signal::unix::Signal);

#[cfg(unix)]
impl Resizes {
    fn new() -> io::Result<Self> {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).map(Self)
    }

    async fn next(&mut self) {
        if self.0.recv().await.is_none() {
            std::future::pending::<()>().await;
        }
    }
}

/// Windows consoles report resizes as input records, not signals; the fake
/// does not report them there.
#[cfg(not(unix))]
struct Resizes;

#[cfg(not(unix))]
impl Resizes {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "same signature as the Unix version"
    )]
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    async fn next(&mut self) {
        std::future::pending::<()>().await;
    }
}
