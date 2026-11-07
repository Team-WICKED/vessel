//! `vessel ui`, the binary: it prints the address with the launch's token
//! and serves the page there; it refuses to listen beyond loopback.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

use super::support::within;

/// `vessel ui` with a private data and configuration directory.
fn ui(dir: &std::path::Path, args: &[&str]) -> Command {
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
        let (base, token) = url.split_once("/#token=").unwrap();
        assert_eq!(token.len(), 64, "{url}");
        let host = base.strip_prefix("http://").unwrap();
        assert!(host.starts_with("127.0.0.1:"), "{url}");
        let mut stream = TcpStream::connect(host).await.unwrap();
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
