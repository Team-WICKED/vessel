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
    // Values that keep the bar (`Limits::check`): four times each
    // per-address limit under half its pool, the unauthenticated share
    // under half of all.
    let mut child = vesseld()
        .args(["relay", "--bind", "127.0.0.1:0", "--token-file"])
        .arg(&token)
        .args([
            "--max-nodes",
            "7",
            "--max-registering",
            "40",
            "--max-clients",
            "70",
        ])
        .args(["--max-clients-per-node", "5", "--max-bytes-per-second", "0"])
        .args([
            "--max-connections",
            "100",
            "--max-connections-per-address",
            "5",
        ])
        .args([
            "--max-registering-per-address",
            "2",
            "--reserved-registering",
            "1",
        ])
        .args([
            "--max-clients-per-address",
            "4",
            "--max-unauthenticated",
            "45",
            "--max-node-clients-per-address",
            "3",
        ])
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
        "max_registering=40",
        "max_clients=70",
        "max_clients_per_node=5",
        "max_bytes_per_second=0",
        "max_connections=100",
        "max_connections_per_address=5",
        "max_registering_per_address=2",
        "reserved_registering=1",
        "max_clients_per_address=4",
        "max_unauthenticated=45",
        "max_node_clients_per_address=3",
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

#[tokio::test]
async fn vesseld_relay_refuses_limits_that_let_one_48_hold_half_a_pool() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("relay.token");
    std::fs::write(&token, "a-relay-token\n").unwrap();
    // The default 16 per address, four times over for a /48, is half of
    // 128 client places.
    let output = tokio::time::timeout(
        SAFETY,
        vesseld()
            .args(["relay", "--bind", "127.0.0.1:0", "--token-file"])
            .arg(&token)
            .args(["--max-clients", "128"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--max-clients-per-address 16")
            && stderr.contains("--max-clients 128")
            && stderr.contains("--max-clients-per-address 15 at most passes")
            && !stderr.contains("relay listening"),
        "{stderr}"
    );
}

#[tokio::test]
async fn vesseld_relay_believes_the_address_header_only_from_trusted_proxies() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("relay.token");
    std::fs::write(&token, "a-relay-token\n").unwrap();
    let run = |extra: &'static [&'static str]| {
        let mut command = vesseld();
        let _command = command
            .args(["relay", "--bind", "127.0.0.1:0", "--token-file"])
            .arg(&token)
            .args(extra);
        command
    };
    // The header alone: anyone could pick their address. Refused.
    let output = tokio::time::timeout(
        SAFETY,
        run(&["--client-address-header", "x-real-ip"]).output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--trusted-proxy"), "{stderr}");
    // Something that is not an address or a network. Refused.
    let output = tokio::time::timeout(
        SAFETY,
        run(&[
            "--client-address-header",
            "x-real-ip",
            "--trusted-proxy",
            "proxy.example",
        ])
        .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("CIDR"), "{stderr}");
    // With proxies named, it serves, and says whom it trusts.
    let mut child = run(&[
        "--client-address-header",
        "x-real-ip",
        "--trusted-proxy",
        "192.0.2.7",
        "--trusted-proxy",
        "10.0.0.0/8",
    ])
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
    assert!(
        limits.contains("192.0.2.7/32") && limits.contains("10.0.0.0/8"),
        "{limits}"
    );
    let _listening = listening_after(&mut lines).await;
    child.kill().await.unwrap();
}

/// The line in which `vesseld relay` says it listens, from its log lines.
#[cfg(test)]
async fn listening_after(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
) -> String {
    tokio::time::timeout(SAFETY, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("relay listening") {
                return line;
            }
        }
        panic!("vesseld relay ended before listening");
    })
    .await
    .unwrap()
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
