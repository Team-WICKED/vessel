//! The handshake: success, the wrong key, an impostor, another version,
//! malformed messages, and the deadline.

use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};

use super::support::{carriers, connected};
use crate::handshake::{read_handshake, responder};
use crate::{Config, NodeKeypair, SecureError, initiate, respond};

#[tokio::test]
async fn both_ends_exchange_frames_after_the_handshake() {
    let (mut client, mut node) = connected().await;
    client.send("hello".to_owned()).await.unwrap();
    assert_eq!(node.next().await.unwrap().unwrap(), "hello");
    node.send("welcome".to_owned()).await.unwrap();
    assert_eq!(client.next().await.unwrap().unwrap(), "welcome");
}

#[tokio::test]
async fn a_client_with_the_wrong_key_is_refused_by_the_node() {
    let keypair = NodeKeypair::generate().unwrap();
    let other = NodeKeypair::generate().unwrap();
    let (client_end, node_end) = carriers();
    let (client, node) = tokio::join!(
        initiate(client_end, other.public()),
        respond(node_end, &keypair),
    );
    assert!(matches!(node, Err(SecureError::Handshake(_))), "{node:?}");
    // NK gives the client nothing to read: the node just closes.
    match client {
        Err(SecureError::Carrier(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        }
        other => panic!("expected the carrier to end, got {other:?}"),
    }
}

#[tokio::test]
async fn an_impostor_answering_for_the_node_fails_at_the_client() {
    let keypair = NodeKeypair::generate().unwrap();
    let (client_end, mut impostor) = carriers();
    let answer = async {
        let _first = impostor.next().await.unwrap().unwrap();
        // Without the node's secret the impostor cannot derive the keys:
        // any 48 bytes it sends fail authentication.
        impostor
            .send(Bytes::from_static(&[7_u8; 48]))
            .await
            .unwrap();
        impostor
    };
    let (client, _impostor) = tokio::join!(initiate(client_end, keypair.public()), answer);
    assert!(
        matches!(client, Err(SecureError::Handshake(snow::Error::Decrypt))),
        "{client:?}"
    );
}

#[tokio::test]
async fn a_node_of_another_version_fails_the_handshake() {
    let keypair = NodeKeypair::generate().unwrap();
    let (client_end, mut node_end) = carriers();
    let other_version = async {
        let first = node_end.next().await.unwrap().unwrap();
        let mut state = responder(b"vessel/secure/2", &keypair).unwrap();
        let read = read_handshake(&mut state, &first);
        drop(node_end);
        read
    };
    let (client, read) = tokio::join!(initiate(client_end, keypair.public()), other_version);
    assert!(matches!(read, Err(SecureError::Handshake(_))), "{read:?}");
    assert!(client.is_err());
}

#[tokio::test]
async fn handshake_messages_of_the_wrong_size_are_refused() {
    let keypair = NodeKeypair::generate().unwrap();

    // A first message with a payload: 49 bytes.
    let (mut client_end, node_end) = carriers();
    client_end
        .send(Bytes::from_static(&[0_u8; 49]))
        .await
        .unwrap();
    let node = respond(node_end, &keypair).await;
    assert!(
        matches!(node, Err(SecureError::HandshakeMessage { len: 49 })),
        "{node:?}"
    );

    // A short answer: 47 bytes.
    let (client_end, mut node_end) = carriers();
    let short = async {
        let _first = node_end.next().await.unwrap().unwrap();
        node_end
            .send(Bytes::from_static(&[0_u8; 47]))
            .await
            .unwrap();
        node_end
    };
    let (client, _node_end) = tokio::join!(initiate(client_end, keypair.public()), short);
    assert!(
        matches!(client, Err(SecureError::HandshakeMessage { len: 47 })),
        "{client:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_deadline_ends_a_handshake_nobody_answers() {
    let keypair = NodeKeypair::generate().unwrap();
    let timeout = Config::default().handshake_timeout;
    assert_eq!(timeout, Duration::from_secs(10));

    // The node never answers (its end stays open).
    let (client_end, _silent_node) = carriers();
    let client = initiate(client_end, keypair.public()).await;
    assert!(
        matches!(client, Err(SecureError::Timeout(after)) if after == timeout),
        "{client:?}"
    );

    // The client never speaks.
    let (_silent_client, node_end) = carriers();
    let node = respond(node_end, &keypair).await;
    assert!(
        matches!(node, Err(SecureError::Timeout(after)) if after == timeout),
        "{node:?}"
    );
}

#[tokio::test]
async fn a_carrier_that_ends_during_the_handshake_is_transient() {
    let keypair = NodeKeypair::generate().unwrap();
    let (client_end, node_end) = carriers();
    drop(node_end);
    let error = initiate(client_end, keypair.public()).await.unwrap_err();
    assert!(error.is_transient(), "{error:?}");
}

#[tokio::test]
async fn a_failed_node_handshake_hands_the_carrier_back() {
    let keypair = NodeKeypair::generate().unwrap();
    let other = NodeKeypair::generate().unwrap();
    let (client_end, node_end) = carriers();
    let client = tokio::spawn(async move { initiate(client_end, other.public()).await });
    let failed = crate::try_respond(node_end, &keypair, &Config::default())
        .await
        .unwrap_err();
    assert!(matches!(failed.error, SecureError::Handshake(_)));
    // The node can still write on it (to say why) before closing.
    let mut carrier = failed.carrier;
    carrier.send(Bytes::from_static(b"why")).await.unwrap();
    drop(carrier);
    assert!(client.await.unwrap().is_err());
}
