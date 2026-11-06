//! `vesseld relay`, the real binary: it starts with a token file and no
//! data directory, answers `/health`, stops on a signal, and refuses to
//! start without a usable token. The relay's behaviour itself is tested in
//! `vessel-relay`.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, Command};

/// Every test waits on events; this only stops a broken one.
#[cfg(test)]
const SAFETY: Duration = Duration::from_secs(60);

#[cfg(test)]
fn vesseld() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vesseld"));
    let _configured = command
        .env("RUST_LOG", "info")
        // No home and no data directory: a relay must not need them.
        .env_remove("HOME")
        .env_remove("VESSEL_DATA_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// The address the relay logs once it listens.
#[cfg(test)]
async fn listening(child: &mut Child) -> String {
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let address = tokio::time::timeout(SAFETY, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("relay listening")
                && let Some(rest) = line.split("address=").nth(1)
            {
                return rest.split_whitespace().next().unwrap().to_owned();
            }
        }
        panic!("vesseld relay ended before listening");
    })
    .await
    .unwrap();
    // Keep draining stderr so the relay never blocks on a full pipe.
    drop(tokio::spawn(async move {
        while let Ok(Some(_line)) = lines.next_line().await {}
    }));
    address
}

#[tokio::test]
async fn vesseld_relay_serves_health_and_stops_on_a_signal() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("relay.token");
    std::fs::write(&token, "a-relay-token\n").unwrap();
    let mut child = vesseld()
        .args(["relay", "--bind", "127.0.0.1:0", "--token-file"])
        .arg(&token)
        .spawn()
        .unwrap();
    let address = listening(&mut child).await;

    let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = String::new();
    let _read = tokio::time::timeout(SAFETY, stream.read_to_string(&mut answer))
        .await
        .unwrap()
        .unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");

    stop(&mut child).await;
}

#[tokio::test]
async fn vesseld_relay_takes_its_limits_from_flags_and_refuses_a_zero_count() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("relay.token");
    std::fs::write(&token, "a-relay-token\n").unwrap();
    let mut child = vesseld()
        .args(["relay", "--bind", "127.0.0.1:0", "--token-file"])
        .arg(&token)
        .args([
            "--max-nodes",
            "7",
            "--max-registering",
            "3",
            "--max-clients",
            "21",
        ])
        .args(["--max-clients-per-node", "5", "--max-bytes-per-second", "0"])
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let limits = tokio::time::timeout(SAFETY, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("relay limits") {
                return line;
            }
        }
        panic!("vesseld relay ended before logging its limits");
    })
    .await
    .unwrap();
    for field in [
        "max_nodes=7",
        "max_registering=3",
        "max_clients=21",
        "max_clients_per_node=5",
        "max_bytes_per_second=0",
    ] {
        assert!(limits.contains(field), "{field}: {limits}");
    }
    child.kill().await.unwrap();

    let output = tokio::time::timeout(
        SAFETY,
        vesseld()
            .args([
                "relay",
                "--bind",
                "127.0.0.1:0",
                "--max-clients",
                "0",
                "--token-file",
            ])
            .arg(&token)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("must be at least 1"), "{stderr}");
}

#[cfg(unix)]
#[cfg(test)]
async fn stop(child: &mut Child) {
    // `kill` rather than a signal crate: this crate has no such dependency.
    let pid = child.id().unwrap().to_string();
    let sent = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .await
        .unwrap();
    assert!(sent.success());
    let status = tokio::time::timeout(SAFETY, child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status}");
}

#[cfg(not(unix))]
#[cfg(test)]
async fn stop(child: &mut Child) {
    child.kill().await.unwrap();
}

#[tokio::test]
async fn vesseld_relay_refuses_an_empty_or_missing_token_file() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.token");
    std::fs::write(&empty, "\n").unwrap();
    for token in [empty, dir.path().join("missing.token")] {
        let output = tokio::time::timeout(
            SAFETY,
            vesseld()
                .args(["relay", "--bind", "127.0.0.1:0", "--token-file"])
                .arg(&token)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("relay token file"), "{stderr}");
    }
}
