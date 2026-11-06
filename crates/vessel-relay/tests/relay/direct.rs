//! A node's own `/secure` endpoint: axum on the node, `connect` on the
//! client, the same handshake and the same `4401` as through a relay.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::response::Response;
use axum::routing::get;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::TcpListener;
use vessel_proto::NodeClose;
use vessel_relay::{AxumCarrier, connect, node_close};
use vessel_secure::{MAX_RECORD, NodeKeypair, SecureError, initiate};

use crate::support::{echo, tls, within};

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
