//! The server's guards, files and websocket hello.

use std::net::SocketAddr;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_util::sync::CancellationToken;
use vessel_web::wire::CLOSE_UNAUTHORIZED;
use vessel_web::{LaunchToken, ServeError, UiConfig};

use crate::support::{Source, Ui, within};

/// A plain HTTP/1.1 exchange: the status code, the headers (lower case),
/// the body.
async fn http(address: SocketAddr, request: &str) -> (u16, Vec<(String, String)>, String) {
    let mut stream = within(TcpStream::connect(address)).await.unwrap();
    within(stream.write_all(request.as_bytes())).await.unwrap();
    let mut said = Vec::new();
    let _read = within(stream.read_to_end(&mut said)).await.unwrap();
    let said = String::from_utf8_lossy(&said).into_owned();
    let (head, body) = said.split_once("\r\n\r\n").unwrap();
    let mut lines = head.lines();
    let code = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect();
    (code, headers, body.to_owned())
}

fn get(path: &str, host: &str, extra: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n")
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header == name)
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn the_page_is_served_with_its_guards() {
    let ui = Ui::start(Source::default()).await;
    let (code, headers, body) = http(ui.address, &get("/", &ui.host(), "")).await;
    assert_eq!(code, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    let csp = header(&headers, "content-security-policy").unwrap();
    assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(header(&headers, "x-frame-options"), Some("DENY"));
    assert_eq!(header(&headers, "referrer-policy"), Some("no-referrer"));
    assert_eq!(
        header(&headers, "cross-origin-opener-policy"),
        Some("same-origin")
    );
    assert_eq!(
        header(&headers, "cross-origin-resource-policy"),
        Some("same-origin")
    );
    assert!(header(&headers, "access-control-allow-origin").is_none());
    assert!(body.contains("<html"));
    // A route of the app is the page too; a missing file is not.
    let (code, _headers, routed) = http(ui.address, &get("/nodes/gpu-box", &ui.host(), "")).await;
    assert_eq!(code, 200);
    assert_eq!(routed, body);
    let (code, _headers, _body) = http(ui.address, &get("/missing.js", &ui.host(), "")).await;
    assert_eq!(code, 404);
    let (code, _headers, _body) = http(ui.address, &get("/api/missing", &ui.host(), "")).await;
    assert_eq!(code, 404);
    // Other names of this machine, with the port, are ours.
    let port = ui.address.port();
    let (code, _headers, _body) =
        http(ui.address, &get("/", &format!("localhost:{port}"), "")).await;
    assert_eq!(code, 200);
}

#[tokio::test]
async fn a_rebinding_site_is_refused() {
    let ui = Ui::start(Source::default()).await;
    let port = ui.address.port();
    for host in [
        format!("evil.example:{port}"),
        "127.0.0.1:1".to_owned(),
        "127.0.0.1".to_owned(),
    ] {
        let (code, headers, _body) = http(ui.address, &get("/", &host, "")).await;
        assert_eq!(code, 421, "{host}");
        assert!(header(&headers, "content-security-policy").is_some());
    }
}

#[tokio::test]
async fn the_api_needs_the_token_and_our_origin() {
    let ui = Ui::start(Source::default()).await;
    let _tab = ui.tab().await;
    let bearer = format!(
        "Authorization: Bearer {}\r\n",
        ui.session.lock().await.as_ref().unwrap()
    );
    let (code, _headers, _body) = http(ui.address, &get("/api/status", &ui.host(), "")).await;
    assert_eq!(code, 401);
    let wrong = "Authorization: Bearer 00\r\n";
    let (code, _headers, _body) = http(ui.address, &get("/api/status", &ui.host(), wrong)).await;
    assert_eq!(code, 401);
    let (code, headers, body) = http(ui.address, &get("/api/status", &ui.host(), &bearer)).await;
    assert_eq!(code, 200);
    assert_eq!(header(&headers, "content-type"), Some("application/json"));
    assert!(body.contains("\"version\""), "{body}");
    // The token, from a page elsewhere, is still refused.
    let elsewhere = format!("{bearer}Origin: http://evil.example\r\n");
    let (code, _headers, _body) =
        http(ui.address, &get("/api/status", &ui.host(), &elsewhere)).await;
    assert_eq!(code, 403);
    let ours = format!("{bearer}Origin: {}\r\n", ui.origin());
    let (code, _headers, _body) = http(ui.address, &get("/api/status", &ui.host(), &ours)).await;
    assert_eq!(code, 200);
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

/// A websocket to the UI with `origin`, or the HTTP status it was refused
/// with.
async fn socket(ui: &Ui, origin: Option<&str>) -> Result<Socket, u16> {
    let mut request = format!("ws://{}/api/ws", ui.address)
        .into_client_request()
        .unwrap();
    if let Some(origin) = origin {
        let _previous = request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
    }
    match within(Box::pin(tokio_tungstenite::connect_async(request))).await {
        Ok((socket, _response)) => Ok(socket),
        Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(error) => panic!("{error}"),
    }
}

/// The close code `socket` ends with.
async fn closed_with(socket: &mut Socket) -> Option<u16> {
    loop {
        match within(socket.next()).await {
            Some(Ok(Message::Close(frame))) => return frame.map(|frame| frame.code.into()),
            Some(Ok(_other)) => {}
            None | Some(Err(_)) => return None,
        }
    }
}

fn hello(token: &str) -> Message {
    Message::Text(format!(r#"{{"type":"hello","token":"{token}"}}"#).into())
}

#[tokio::test]
async fn a_websocket_from_elsewhere_or_without_origin_is_refused() {
    let ui = Ui::start(Source::default()).await;
    assert_eq!(
        socket(&ui, Some("http://evil.example")).await.err(),
        Some(403)
    );
    assert_eq!(socket(&ui, None).await.err(), Some(403));
}

#[tokio::test]
async fn a_websocket_must_present_the_token_first_and_in_time() {
    let ui = Ui::start(Source::default()).await;
    let origin = ui.origin();
    // The wrong token.
    let mut wrong = socket(&ui, Some(&origin)).await.unwrap();
    within(wrong.send(hello("00"))).await.unwrap();
    assert_eq!(closed_with(&mut wrong).await, Some(CLOSE_UNAUTHORIZED));
    // Something else first.
    let mut other = socket(&ui, Some(&origin)).await.unwrap();
    within(other.send(Message::Text(r#"{"type":"welcome"}"#.into())))
        .await
        .unwrap();
    assert_eq!(closed_with(&mut other).await, Some(CLOSE_UNAUTHORIZED));
    // Nothing at all, past the deadline.
    let mut silent = socket(&ui, Some(&origin)).await.unwrap();
    assert_eq!(closed_with(&mut silent).await, Some(CLOSE_UNAUTHORIZED));
    // The new handshake proves both peers (the helper verifies the HMAC).
    let _tab = ui.tab().await;
}

#[tokio::test]
async fn the_ui_listens_on_loopback_only() {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let config = UiConfig::new(LaunchToken::mint().unwrap());
    let served = within(vessel_web::serve(
        listener,
        config,
        Source::default(),
        CancellationToken::new(),
    ))
    .await;
    assert!(
        matches!(served, Err(ServeError::NotLoopback(_))),
        "{served:?}"
    );
}

#[tokio::test]
async fn oversized_preauth_frame_is_rejected_from_its_header() {
    within(async {
        let ui = Ui::configured(Source::default(), |config| {
            config.hello_deadline = std::time::Duration::from_secs(30);
        })
        .await;
        let mut pending = socket(&ui, Some(&ui.origin())).await.unwrap();
        let _challenge = pending.next().await.unwrap().unwrap();
        let tokio_tungstenite::MaybeTlsStream::Plain(raw) = pending.get_mut() else {
            panic!("not plain")
        };
        // A masked 8 KiB frame header, with no payload. Refuse before
        // allocating/reading that payload, not at the 30-second deadline.
        raw.write_all(&[0x82, 0xfe, 0x20, 0, 1, 2, 3, 4])
            .await
            .unwrap();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(2), pending.next()).await;
        assert!(matches!(
            ended,
            Ok(None | Some(Err(_) | Ok(Message::Close(_))))
        ));
        // Authentication grows the limit: ordinary large requests survive.
        let mut tab = ui.tab().await;
        let id = tab
            .op(vessel_web::wire::Op::Inspect {
                link: "x".repeat(8192),
            })
            .await;
        tab.until(|frame| {
            matches!(frame, vessel_web::wire::ServerFrame::OpReply { id: got, .. } if got == id)
                .then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test]
async fn fragmented_preauth_messages_are_bounded_too() {
    use tokio_tungstenite::tungstenite::protocol::{
        Message,
        frame::{
            Frame,
            coding::{Data, OpCode},
        },
    };
    let ui = Ui::configured(Source::default(), |config| {
        config.hello_deadline = std::time::Duration::from_secs(30);
    })
    .await;
    let mut pending = socket(&ui, Some(&ui.origin())).await.unwrap();
    let _challenge = pending.next().await.unwrap().unwrap();
    pending
        .send(Message::Frame(Frame::message(
            vec![b' '; 3000],
            OpCode::Data(Data::Text),
            false,
        )))
        .await
        .unwrap();
    pending
        .send(Message::Frame(Frame::message(
            vec![b' '; 3000],
            OpCode::Data(Data::Continue),
            true,
        )))
        .await
        .unwrap();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), pending.next()).await;
    assert!(matches!(
        closed,
        Ok(None | Some(Err(_) | Ok(Message::Close(_))))
    ));
}

#[tokio::test]
async fn idle_and_partial_http_headers_have_a_deadline() {
    let ui = Ui::configured(Source::default(), |config| {
        config.header_deadline = std::time::Duration::from_millis(100);
    })
    .await;
    for prefix in ["", "GET / HTTP/1.1\r\nHost: "] {
        let mut stream = TcpStream::connect(ui.address).await.unwrap();
        stream.write_all(prefix.as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        let closed = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.read_to_end(&mut bytes),
        )
        .await;
        assert!(closed.is_ok(), "idle HTTP connection was retained");
    }
}

#[tokio::test]
async fn unauthenticated_and_total_caps_include_upgraded_sockets() {
    use vessel_web::wire::{Op, ServerFrame};
    let ui = Ui::configured(Source::default(), |config| {
        config.max_connections = 3;
        config.max_unauthenticated = 2;
        config.hello_deadline = std::time::Duration::from_secs(30);
    })
    .await;
    let mut first = ui.tab().await;
    let _second = ui.tab().await;
    let mut pending = socket(&ui, Some(&ui.origin())).await.unwrap();
    let _challenge = pending.next().await.unwrap().unwrap();
    let mut excess = TcpStream::connect(ui.address).await.unwrap();
    let mut byte = [0];
    assert!(matches!(
        within(excess.read(&mut byte)).await,
        Ok(0) | Err(_)
    ));
    let id = first.op(Op::Nodes).await;
    first
        .until(|frame| {
            matches!(frame, ServerFrame::OpReply { id: got, .. } if got == id).then_some(())
        })
        .await;
    // Separately fill only the pending cap, leaving total places free.
    let ui = Ui::configured(Source::default(), |config| {
        config.max_connections = 8;
        config.max_unauthenticated = 1;
        config.hello_deadline = std::time::Duration::from_secs(30);
    })
    .await;
    let mut pending = socket(&ui, Some(&ui.origin())).await.unwrap();
    let _challenge = pending.next().await.unwrap().unwrap();
    let mut excess = TcpStream::connect(ui.address).await.unwrap();
    assert!(matches!(
        within(excess.read(&mut byte)).await,
        Ok(0) | Err(_)
    ));
}
