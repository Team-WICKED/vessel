//! `vessel ui`, the binary: it prints the address with the launch's token
//! and serves the page there; it refuses to listen beyond loopback.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

use super::support::within;

/// `vessel ui` with a private data and configuration directory.
fn ui(dir: &std::path::Path, args: &[&str]) -> Command {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        let _command = command.args([
            "-c",
            "umask 000; exec \"$@\"",
            "sh",
            env!("CARGO_BIN_EXE_vessel"),
        ]);
        command
    };
    #[cfg(not(unix))]
    let mut command = Command::new(env!("CARGO_BIN_EXE_vessel"));
    let _ = command
        .arg("--data-dir")
        .arg(dir)
        .arg("ui")
        .args(args)
        .env("VESSEL_CONFIG_DIR", dir.join("config"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn vessel_ui_prints_its_address_and_serves_the_page_there() {
    within(async {
        let dir = tempfile::tempdir().unwrap();
        let mut child = ui(dir.path(), &["--no-open", "--port", "0"])
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let url = loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("vessel ui ended first");
            if let Some(url) = line.strip_prefix("Vessel UI: ") {
                break url.to_owned();
            }
        };
        let (base, token) = url.split_once("/#bootstrap=").unwrap();
        assert_eq!(token.len(), 64, "{url}");
        let host = base.strip_prefix("http://").unwrap();
        assert!(host.starts_with("vessel-") && host.contains(".localhost:"));
        let port = host.rsplit_once(':').unwrap().1.parse::<u16>().unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut said = String::new();
        let _read = stream.read_to_string(&mut said).await.unwrap();
        assert!(said.starts_with("HTTP/1.1 200"), "{said}");
        assert!(said.contains("<html"), "{said}");
        child.kill().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn vessel_ui_listens_on_loopback_only() {
    within(async {
        let dir = tempfile::tempdir().unwrap();
        let output = ui(dir.path(), &["--no-open", "--bind", "0.0.0.0"])
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("loopback"), "{stderr}");
    })
    .await;
}

/// Regression for the default opener: no other Unix user can traverse
/// the launch directory, and normal termination removes the capability.
#[cfg(unix)]
#[tokio::test]
async fn opener_is_private_and_removed_on_termination() {
    use std::os::unix::fs::PermissionsExt as _;
    within(async {
        for signal in ["INT", "TERM", "HUP"] {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            let name = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let stub = bin.join(name);
            // A pipe acknowledgement avoids sleeping/racing the opener.
            std::fs::write(
                &stub,
                "#!/bin/sh\nprintf '%s\\n' \"$1\" > \"$XDG_RUNTIME_DIR/opened\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
            let fifo = dir.path().join("opened");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            let reading =
                tokio::task::spawn_blocking(move || std::fs::read_to_string(fifo).unwrap());
            let mut command = ui(dir.path(), &["--port", "0"]);
            let _ = command.env("PATH", &bin).env("XDG_RUNTIME_DIR", dir.path());
            let mut child = command.spawn().unwrap();
            let file = std::path::PathBuf::from(reading.await.unwrap().trim());
            let parent = file.parent().unwrap();
            assert_eq!(parent.parent(), Some(dir.path()));
            assert_eq!(
                std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let page = std::fs::read_to_string(&file).unwrap();
            assert!(page.contains(".localhost:"));
            assert!(
                std::process::Command::new("kill")
                    .args([format!("-{signal}"), child.id().unwrap().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(child.wait().await.unwrap().success());
            assert!(!parent.exists());
        }
    })
    .await;
}
