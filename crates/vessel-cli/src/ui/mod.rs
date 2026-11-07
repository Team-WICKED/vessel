//! `vessel ui`: the web UI on this machine (ADR 0017).
//!
//! [Decision Log]
//! - Unix opens a private file rather than a capability URL in argv:
//!   process lists are visible to other local users. Directory/file modes
//!   are 0700/0600 at creation, under `XDG_RUNTIME_DIR` when set. Registered
//!   SIGINT/SIGTERM/SIGHUP handlers remove it; forced kills cannot promise
//!   cleanup. Only a one-use bootstrap code reaches disk, not a session key.
//! - Windows prints the URL without an opener file until we have an atomic
//!   protected ACL implementation. A configurable temp path is not proof
//!   of privacy. The terminal fallback also handles opener failures on Unix.
//! - The default port is 7079, or a free port when taken. Port reuse is not
//!   an identity guarantee: mutual proofs pin a launch; SRI authenticates
//!   executable assets, and a random `.localhost` origin per launch prevents
//!   old cached HTML and service workers from intercepting that index.

use std::net::{IpAddr, SocketAddr};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use vessel_web::{LaunchToken, UiConfig};

use crate::args::{DEFAULT_UI_PORT, Target, UiArgs};
use crate::error::CliError;
use crate::io;

mod launch;
mod source;

/// Serve the UI until interrupted, bridging to the book's nodes and this
/// machine's.
pub(crate) async fn ui(target: &Target, args: UiArgs) -> Result<(), CliError> {
    if !args.bind.is_loopback() {
        return Err(CliError::Usage(
            "--bind takes a loopback address (127.0.0.1 or ::1): the UI is for this machine",
        ));
    }
    // Install all termination handlers before publishing a capability.
    let mut signals = launch::Signals::new()?;
    let listener = listen(args.bind, args.port).await?;
    let token = LaunchToken::mint()?;
    let url = token.url(listener.local_addr()?);
    io::out(
        format!("Vessel UI: {url}\nThe address holds a one-use code: keep it to yourself and keep the tab open.\n")
            .as_bytes(),
    )
    .await?;
    // Held until the UI stops: the browser reads the file when it opens.
    let _opening = if args.no_open { None } else { open(&url).await };
    let shutdown = CancellationToken::new();
    let source = source::BookSource::new(target);
    let serving = vessel_web::serve(listener, UiConfig::new(token), source, shutdown.clone());
    tokio::select! {
        served = serving => Ok(served?),
        interrupted = signals.wait() => {
            shutdown.cancel();
            Ok(interrupted?)
        }
    }
}

/// A listener on `bind`: `port`, or any free one when the default is
/// taken.
async fn listen(bind: IpAddr, port: u16) -> Result<TcpListener, CliError> {
    match TcpListener::bind(SocketAddr::new(bind, port)).await {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && port == DEFAULT_UI_PORT => {
            Ok(TcpListener::bind(SocketAddr::new(bind, 0)).await?)
        }
        Err(error) => Err(error.into()),
    }
}

/// Open the browser on `url` through a file only this user can read (see
/// the module's notes); the directory holding it, to keep while the UI
/// runs. Best effort: when anything fails, the printed URL is still
/// there.
async fn open(url: &str) -> Option<tempfile::TempDir> {
    let url = url.to_owned();
    let dir = match tokio::task::spawn_blocking(move || launch::file(&url))
        .await
        .ok()?
    {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(%error, "could not create a private browser handoff; open the printed address");
            return None;
        }
    };
    let file = dir.path().join("open.html");
    let mut opener = opener(&file);
    let spawned = opener
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            // Reaped in the background: some openers wait for the browser.
            drop(tokio::spawn(async move {
                let _status = child.wait().await;
            }));
            Some(dir)
        }
        Err(error) => {
            tracing::warn!(%error, "could not open the browser; open the address above");
            None
        }
    }
}

/// The platform's command that opens `file` in the default browser.
fn opener(file: &std::path::Path) -> tokio::process::Command {
    if cfg!(target_os = "macos") {
        let mut command = tokio::process::Command::new("open");
        let _command = command.arg(file);
        command
    } else if cfg!(windows) {
        let mut command = tokio::process::Command::new("cmd");
        let _command = command.args(["/C", "start", ""]).arg(file);
        command
    } else {
        let mut command = tokio::process::Command::new("xdg-open");
        let _command = command.arg(file);
        command
    }
}
