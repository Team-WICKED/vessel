//! Connected channel pairs, with or without a tap in the middle.

use std::num::NonZeroUsize;

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};

use crate::{
    Config, MemoryCarrier, NodeKeypair, SecureChannel, SecureError, initiate_with, memory_pair,
    respond_with,
};

/// Records each in-memory direction holds.
const CAPACITY: NonZeroUsize = NonZeroUsize::new(64).unwrap();

pub(super) type Channel = SecureChannel<MemoryCarrier>;

pub(super) fn hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex: {text}");
    text.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

pub(super) fn bytes32(text: &str) -> [u8; 32] {
    hex(text).try_into().unwrap()
}

pub(super) fn carriers() -> (MemoryCarrier, MemoryCarrier) {
    memory_pair(CAPACITY)
}

/// A client and a node with a fresh key, after the handshake.
pub(super) async fn connected() -> (Channel, Channel) {
    connected_with(&Config::default(), &Config::default()).await
}

pub(super) async fn connected_with(client: &Config, node: &Config) -> (Channel, Channel) {
    let keypair = NodeKeypair::generate().unwrap();
    let (client_end, node_end) = carriers();
    let (client, node) = tokio::join!(
        initiate_with(client_end, keypair.public(), client),
        respond_with(node_end, &keypair, node),
    );
    (client.unwrap(), node.unwrap())
}

/// What sits between a client and a node: the test reads every record the
/// client sends and decides what the node gets.
pub(super) struct Middle {
    client_side: MemoryCarrier,
    node_side: MemoryCarrier,
}

impl Middle {
    /// The next record the client sent.
    pub(super) async fn client_record(&mut self) -> Bytes {
        self.client_side.next().await.unwrap().unwrap()
    }

    /// Hand `record` to the node.
    pub(super) async fn pass_to_node(&mut self, record: Bytes) {
        self.node_side.send(record).await.unwrap();
    }
}

/// A client and a node whose records all pass through a [`Middle`]; the
/// handshake is forwarded as is.
pub(super) async fn through_middle() -> (Channel, Channel, Middle) {
    let keypair = NodeKeypair::generate().unwrap();
    let (client_end, client_side) = carriers();
    let (node_side, node_end) = carriers();
    let mut middle = Middle {
        client_side,
        node_side,
    };
    let config = Config::default();
    let forward = async {
        let first = middle.client_record().await;
        middle.pass_to_node(first).await;
        let second = middle.node_side.next().await.unwrap().unwrap();
        middle.client_side.send(second).await.unwrap();
    };
    let (client, node, ()) = tokio::join!(
        initiate_with(client_end, keypair.public(), &config),
        respond_with(node_end, &keypair, &config),
        forward,
    );
    (client.unwrap(), node.unwrap(), middle)
}

/// The rule a tampered channel reports.
pub(super) fn tamper(result: Option<Result<String, SecureError>>) -> crate::Tamper {
    match result {
        Some(Err(SecureError::Tampered(rule))) => rule,
        other => panic!("expected a tampering error, got {other:?}"),
    }
}

/// After any failure the channel is closed: the stream ends and sends fail.
pub(super) async fn assert_closed(channel: &mut Channel) {
    assert!(channel.next().await.is_none());
    assert!(matches!(
        channel.send("after".to_owned()).await,
        Err(SecureError::Closed)
    ));
}
