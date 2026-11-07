//! `vessel ui`: the web UI on this machine (ADR 0017).
//!
//! [Decision Log]
//! - The browser is opened on a file, not on the URL. The URL holds the
//!   launch's token, and a command line (the opener's, the browser's) can
//!   be read by every user of the machine (`ps`, `/proc/*/cmdline`). The
//!   file sits in a temporary directory only this user can read, sends the
//!   browser on to the URL, and goes when the UI does. Jupyter does the
//!   same for the same reason.
//! - The URL is also printed, for `--no-open` and for a browser that did
//!   not open: the terminal is the user's.
//! - The default port is 7079 (`vesseld` has 7077, `vesseld relay` 7078);
//!   when it is taken, any free port. The token changes with every launch
//!   anyway, so nothing depends on the port staying the same.

use std::net::{IpAddr, SocketAddr};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use vessel_web::{LaunchToken, UiConfig};

use crate::args::{DEFAULT_UI_PORT, Target, UiArgs};
use crate::error::CliError;
use crate::io;

mod source;

/// Serve the UI until interrupted, bridging to the book's nodes and this
/// machine's.
pub(crate) async fn ui(target: &Target, args: UiArgs) -> Result<(), CliError> {
    if !args.bind.is_loopback() {
        return Err(CliError::Usage(
            "--bind takes a loopback address (127.0.0.1 or ::1): the UI is for this machine",
        ));
    }
    let listener = listen(args.bind, args.port).await?;
    let token = LaunchToken::mint()?;
    let url = token.url(listener.local_addr()?);
    io::out(
        format!("Vessel UI: {url}\nThe address holds this launch's key: keep it to yourself.\n")
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
        interrupted = tokio::signal::ctrl_c() => {
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
    let dir = tempfile::Builder::new()
        .prefix("vessel-ui-")
        .tempdir()
        .ok()?;
    let file = dir.path().join("open.html");
    let page = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0; url={url}\">\
         <title>Vessel</title><a href=\"{url}\">Open Vessel</a>\n"
    );
    tokio::fs::write(&file, page).await.ok()?;
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
