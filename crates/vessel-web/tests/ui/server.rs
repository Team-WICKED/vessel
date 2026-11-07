//! The server's guards, files and websocket hello.

use std::net::SocketAddr;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_util::sync::CancellationToken;
use vessel_web::wire::{CLOSE_UNAUTHORIZED, ServerFrame};
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
    let bearer = format!("Authorization: Bearer {}\r\n", ui.secret());
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
    // The token: welcome.
    let mut good = socket(&ui, Some(&origin)).await.unwrap();
    within(good.send(hello(&ui.secret()))).await.unwrap();
    let welcome = loop {
        match within(good.next()).await {
            Some(Ok(Message::Text(text))) => break text,
            Some(Ok(_other)) => {}
            other => panic!("expected a welcome, got {other:?}"),
        }
    };
    let frame: ServerFrame = serde_json::from_str(&welcome).unwrap();
    assert!(matches!(frame, ServerFrame::Welcome { .. }), "{frame:?}");
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
