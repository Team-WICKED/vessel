//! A node's own `/secure` endpoint: axum on the node, `connect` on the
//! client, the same handshake and the same `4401` as through a relay; and
//! a node's carriers closing with a code while the other side still sends
//! (they read until its close, so the close is not lost to a reset).

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{self, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::{self, Message};
use vessel_proto::NodeClose;
use vessel_relay::{AxumCarrier, NodeCarrier as _, connect, node_close};
use vessel_secure::{MAX_RECORD, NodeKeypair, SecureError, initiate};

use crate::support::{closed, echo, tls, within};

async fn secure(State(keypair): State<Arc<NodeKeypair>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(MAX_RECORD)
        .max_frame_size(MAX_RECORD)
        .on_upgrade(move |socket| async move {
            echo(AxumCarrier::new(socket, None), &keypair).await;
        })
}

/// A node serving `/secure`; its URL.
async fn node(keypair: Arc<NodeKeypair>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/secure", get(secure))
        .with_state(keypair);
    drop(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    format!("ws://{address}/secure")
}

#[tokio::test]
async fn a_client_reaches_a_nodes_own_secure_endpoint() {
    let keypair = Arc::new(NodeKeypair::generate().unwrap());
    let url = node(Arc::clone(&keypair)).await;
    let carrier = within(connect(&url, &tls())).await.unwrap();
    let mut channel = within(initiate(carrier, keypair.public())).await.unwrap();
    let long = "y".repeat(150_000);
    within(channel.send(long.clone())).await.unwrap();
    assert!(within(channel.next()).await.unwrap().unwrap() == long);
}

#[tokio::test]
async fn a_wrong_pinned_key_is_told_with_4401() {
    let keypair = Arc::new(NodeKeypair::generate().unwrap());
    let url = node(keypair).await;
    let carrier = within(connect(&url, &tls())).await.unwrap();
    let other = NodeKeypair::generate().unwrap();
    let error = within(initiate(carrier, other.public())).await.unwrap_err();
    let SecureError::Carrier(io) = &error else {
        panic!("expected the node's close, got {error:?}");
    };
    assert_eq!(node_close(io), Some(NodeClose::HandshakeFailed));
}

#[tokio::test]
async fn a_failed_handshake_is_told_with_4401_while_the_client_still_sends() {
    let keypair = Arc::new(NodeKeypair::generate().unwrap());
    let url = node(keypair).await;
    let (socket, _response) = within(Box::pin(tokio_tungstenite::connect_async(&url)))
        .await
        .unwrap();
    let (mut sink, mut stream) = socket.split();
    // Not a handshake message, then records the node never reads: closed
    // with them unread, its socket would be reset and the 4401 lost.
    let sending = async {
        let mut records = std::iter::once(Bytes::from(vec![0; 48]))
            .chain(std::iter::repeat_n(Bytes::from(vec![0; 60 * 1024]), 128));
        records.try_for_each_send(&mut sink).await
    };
    let (sent, (code, end)) = tokio::join!(sending, closed(&mut stream));
    assert_eq!(code, Some(NodeClose::HandshakeFailed.code()));
    assert_eq!(sent, Ok(()), "the node reset the connection");
    assert_eq!(end, Ok(()), "the node reset the connection");
}

/// How the other side saw a close: its code, how its reading ended, and
/// whether its sending met a reset.
type Seen = (Option<u16>, Result<(), String>, Result<(), String>);

/// A websocket endpoint that sends records as fast as it can while it
/// reads, and reports what it saw of the close through `report`.
async fn flood(
    State(report): State<Arc<Mutex<Option<oneshot::Sender<Seen>>>>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| async move {
        let (mut sink, mut stream) = socket.split();
        let sending = async {
            for _ in 0..128 {
                if let Err(error) = sink
                    .send(ws::Message::Binary(vec![0; 60 * 1024].into()))
                    .await
                {
                    return reset_or_closing(&error);
                }
            }
            Ok(())
        };
        let reading = async {
            let code = loop {
                match stream.next().await {
                    Some(Ok(ws::Message::Close(frame))) => break frame.map(|frame| frame.code),
                    Some(Ok(_other)) => {}
                    None => return (None, Err("ended without a close".to_owned())),
                    Some(Err(error)) => return (None, Err(format!("before the close: {error}"))),
                }
            };
            let end = match stream.next().await {
                None => Ok(()),
                Some(Ok(message)) => Err(format!("after the close: {message:?}")),
                Some(Err(error)) => Err(format!("after the close: {error}")),
            };
            (code, end)
        };
        let (sent, (code, end)) = tokio::join!(sending, reading);
        if let Some(report) = report.lock().unwrap().take() {
            let _reported = report.send((code, end, sent));
        }
    })
}

/// `Ok` for a send refused because the close was under way, the error
/// for an I/O failure (a reset).
fn reset_or_closing(error: &axum::Error) -> Result<(), String> {
    let io = std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<tungstenite::Error>())
        .is_some_and(|error| matches!(error, tungstenite::Error::Io(_)));
    if io { Err(error.to_string()) } else { Ok(()) }
}

#[tokio::test]
async fn a_carrier_closing_with_a_code_while_the_other_side_sends_is_heard() {
    let (report, seen) = oneshot::channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/flood", get(flood))
        .with_state(Arc::new(Mutex::new(Some(report))));
    drop(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    // The node's end of a relay's data connection, as a `WsCarrier`: one
    // record read, then a close with a code, the rest never read.
    let mut carrier = within(connect(&format!("ws://{address}/flood"), &tls()))
        .await
        .unwrap();
    let _first = within(carrier.next()).await.unwrap().unwrap();
    within(carrier.close_with(NodeClose::HandshakeFailed)).await;
    let (code, end, sent) = within(seen).await.unwrap();
    assert_eq!(code, Some(NodeClose::HandshakeFailed.code()));
    assert_eq!(end, Ok(()), "the carrier reset the connection");
    assert_eq!(sent, Ok(()), "the carrier reset the connection");
}

/// Sending records one by one until done or refused.
trait SendAll {
    /// `Ok` when all went, or a send was refused because the close was
    /// under way; the error for an I/O failure (a reset).
    async fn try_for_each_send<S>(&mut self, sink: &mut S) -> Result<(), String>
    where
        S: futures_util::Sink<Message, Error = tungstenite::Error> + Unpin;
}

impl<I: Iterator<Item = Bytes>> SendAll for I {
    async fn try_for_each_send<S>(&mut self, sink: &mut S) -> Result<(), String>
    where
        S: futures_util::Sink<Message, Error = tungstenite::Error> + Unpin,
    {
        for record in self {
            if let Err(error) = sink.send(Message::Binary(record)).await {
                return if let tungstenite::Error::Io(io) = error {
                    Err(io.to_string())
                } else {
                    Ok(())
                };
            }
        }
        Ok(())
    }
}
