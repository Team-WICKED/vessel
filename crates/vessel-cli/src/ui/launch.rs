//! Private browser handoff and termination. Creation modes, not a later
//! chmod, protect the capability even with a permissive umask.
use std::io;

#[cfg(unix)]
pub(super) fn file(url: &str) -> io::Result<tempfile::TempDir> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut builder = tempfile::Builder::new();
    let _builder = builder.prefix("vessel-ui-");
    let _builder = builder.permissions(std::fs::Permissions::from_mode(0o700));
    let root = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let dir = match root {
        Some(root) => builder.tempdir_in(root)?,
        None => builder.tempdir()?,
    };
    let mut options = std::fs::OpenOptions::new();
    let _options = options.write(true).create_new(true);
    let _options = options.mode(0o600);
    let mut file = options.open(dir.path().join("open.html"))?;
    writeln!(
        file,
        "<!doctype html><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0; url={url}\"><title>Vessel</title><a href=\"{url}\">Open Vessel</a>"
    )?;
    Ok(dir)
}

// A configurable Windows TEMP directory need not have a private ACL.
// Until we can create an explicit protected DACL atomically, fail closed:
// the UI still works via its printed address and writes no handoff secret.
#[cfg(not(unix))]
pub(super) fn file(_url: &str) -> io::Result<tempfile::TempDir> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private browser handoff is unavailable on this platform; open the printed address",
    ))
}

/// Registered before creating the handoff file, so SIGTERM/SIGHUP cannot
/// race a lazily-polled signal future and leave the file behind.
pub(super) struct Signals {
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
}
impl Signals {
    pub(super) fn new() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
                hangup: signal(SignalKind::hangup())?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c()?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "termination signals are unavailable",
            ))
        }
    }
    pub(super) async fn wait(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => Ok(()),
            _ = self.terminate.recv() => Ok(()),
            _ = self.hangup.recv() => Ok(()),
        }
        #[cfg(windows)]
        {
            let _signal = self.ctrl_c.recv().await;
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "termination signals are unavailable",
            ))
        }
    }
}
