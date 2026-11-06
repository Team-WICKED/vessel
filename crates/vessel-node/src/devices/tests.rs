//! Devices end to end inside the node: invitations, the preamble on a
//! channel (an in-memory pair standing in for a secure channel), the scope
//! check, and revocation closing connections.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use futures_util::{SinkExt as _, StreamExt as _};
use tempfile::TempDir;
use vessel_client::pairing::{self, PreambleError};
use vessel_client::secure;
use vessel_harness::Lookup;
use vessel_proto::{
    ClientInfo, CreateSession, DeviceId, DeviceInfo, DeviceSecret, ErrorCode, HarnessKind,
    InviteDevice, ListDevices, ListSessions, NodeClose, NodePublicKey, PairingOffer, Preamble,
    PreambleReply, RefusalReason, RevokeDevice, Scope, SessionMode, SessionSpec, TerminalSize,
};
use vessel_relay::NodeCarrier;
use vessel_rpc::memory::{self, MemoryTransport};
use vessel_rpc::{CallError, ClientConfig, Connection};
use vessel_secure::{MemoryCarrier, NodeKeypair, SecureChannel, SecureError, memory_pair};
use vessel_store::{CredentialHash, DataDir};

use super::*;
use crate::{Config, Node};

async fn node() -> (TempDir, Node) {
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config::new(DataDir::new(temp.path().join("data")));
    config.bind = None;
    config.harness_lookup = Lookup::new(Vec::<PathBuf>::new());
    config.relays = vec!["wss://relay.example/vessel".parse().unwrap()];
    config.relay_token_file = Some(temp.path().join("relay-token"));
    std::fs::write(temp.path().join("relay-token"), "shared").unwrap();
    config.public = vec!["ws://203.0.113.5:7077".parse().unwrap()];
    let node = Node::start(config).await.unwrap();
    (temp, node)
}

fn client() -> ClientInfo {
    ClientInfo {
        name: "devices-test".to_owned(),
        version: "0".to_owned(),
    }
}

/// A channel to the node's device gateway, as a secure channel would be.
fn channel(node: &Node) -> MemoryTransport {
    let (client_end, node_end) = memory::pair(NonZeroUsize::new(16).unwrap());
    let gateway = node.secure.clone();
    drop(tokio::spawn(async move { gateway.serve(node_end).await }));
    client_end
}

/// Send `preamble` and read the node's answer.
async fn preamble(channel: &mut MemoryTransport, preamble: &Preamble) -> PreambleReply {
    channel
        .send(serde_json::to_string(preamble).unwrap())
        .await
        .unwrap();
    let reply = channel.next().await.unwrap().unwrap();
    serde_json::from_str(&reply).unwrap()
}

fn invite(node: &Node, label: Option<&str>) -> PairingOffer {
    node.secure
        .devices
        .invite(&InviteDevice {
            label: label.map(str::to_owned),
            ttl_secs: 600,
        })
        .unwrap()
        .link
        .decode()
        .unwrap()
}

/// Pair through a fresh channel with the client's own `pair`; returns the
/// device, its secret, and the protocol connection that follows on the same
/// channel.
async fn pair(node: &Node, offer: PairingOffer) -> (DeviceId, DeviceSecret, Connection) {
    let (paired, channel) = pairing::pair(channel(node), &offer, "laptop")
        .await
        .unwrap();
    assert_eq!(paired.node.id, node.info().id);
    let connection = Connection::connect(channel, client(), &ClientConfig::default())
        .await
        .unwrap();
    (
        paired.credential.device,
        paired.credential.secret,
        connection,
    )
}

/// Authenticate through a fresh channel with the client's `authenticate`.
async fn authenticate(
    node: &Node,
    device: DeviceId,
    secret: &DeviceSecret,
) -> Result<Connection, RefusalReason> {
    let info = node.info();
    let key = info.public_key.unwrap();
    let credential = pairing::credential(info.id, key, device, secret.clone());
    match pairing::authenticate(channel(node), &credential).await {
        Ok(channel) => Ok(
            Connection::connect(channel, client(), &ClientConfig::default())
                .await
                .unwrap(),
        ),
        Err(PreambleError::Refused(reason)) => Err(reason),
        Err(other) => panic!("expected an answer, got {other}"),
    }
}

#[tokio::test]
async fn an_invitation_is_a_link_to_this_node_with_its_routes() {
    let (_temp, node) = node().await;
    let offer = invite(&node, Some("phone"));
    assert_eq!(offer.node, node.info().id);
    assert_eq!(Some(offer.key), node.info().public_key);
    assert_eq!(offer.relays[0].as_str(), "wss://relay.example/vessel");
    assert_eq!(offer.direct[0].as_str(), "ws://203.0.113.5:7077");
    let ttl = offer.expires - Utc::now();
    assert!(ttl > TimeDelta::seconds(590) && ttl <= TimeDelta::seconds(600));

    let capped = node
        .secure
        .devices
        .invite(&InviteDevice {
            label: None,
            ttl_secs: u32::MAX,
        })
        .unwrap();
    assert!(capped.expires - Utc::now() <= MAX_INVITATION_TTL);
    assert!(matches!(
        node.secure.devices.invite(&InviteDevice {
            label: None,
            ttl_secs: 0
        }),
        Err(ServiceError::InvalidTtl)
    ));
    node.stop().await.unwrap();
}

/// The node's key is `node.key`'s: the same across restarts, and published
/// in `NodeInfo` for clients already connected (token or SSH).
#[tokio::test]
async fn the_node_key_comes_from_node_key_and_survives_a_restart() {
    let temp = tempfile::tempdir().unwrap();
    let start = || {
        let mut config = Config::new(DataDir::new(temp.path()));
        config.bind = None;
        Node::start(config)
    };
    let node = start().await.unwrap();
    let key = node.info().public_key.unwrap();
    let secret = DataDir::new(temp.path()).read_node_key().unwrap();
    assert_eq!(&key, NodeKeypair::from_secret(*secret.expose()).public());
    node.stop().await.unwrap();
    let again = start().await.unwrap();
    assert_eq!(again.info().public_key, Some(key));
    again.stop().await.unwrap();
}

/// An in-memory carrier the node can close with a code, as it closes
/// websockets: the code is recorded for the test to read.
struct Closable {
    carrier: MemoryCarrier,
    closed: Arc<Mutex<Option<NodeClose>>>,
}

impl futures_util::Stream for Closable {
    type Item = std::io::Result<bytes::Bytes>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.get_mut().carrier).poll_next(cx)
    }
}

impl futures_util::Sink<bytes::Bytes> for Closable {
    type Error = std::io::Error;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().carrier).poll_ready(cx)
    }

    fn start_send(self: std::pin::Pin<&mut Self>, item: bytes::Bytes) -> std::io::Result<()> {
        std::pin::Pin::new(&mut self.get_mut().carrier).start_send(item)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().carrier).poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().carrier).poll_close(cx)
    }
}

impl NodeCarrier for Closable {
    async fn close_with(self, close: NodeClose) {
        *self.closed.lock().unwrap() = Some(close);
    }
}

/// The node's gateway answering on one end of a new in-memory pair; the
/// other end, and how the node closed it (if it did).
fn carrier(node: &Node) -> (MemoryCarrier, Arc<Mutex<Option<NodeClose>>>) {
    let (client_end, node_end) = memory_pair(NonZeroUsize::new(16).unwrap());
    let closed = Arc::new(Mutex::new(None));
    let node_end = Closable {
        carrier: node_end,
        closed: Arc::clone(&closed),
    };
    let gateway = node.secure.clone();
    drop(tokio::spawn(async move { gateway.accept(node_end).await }));
    (client_end, closed)
}

/// A secure channel to the node: a carrier the gateway answers with the
/// node's key, and the client's end after its handshake to `key`.
async fn secure(
    node: &Node,
    key: &NodePublicKey,
) -> (
    Result<SecureChannel<MemoryCarrier>, SecureError>,
    tokio::task::JoinHandle<Result<(), SecureError>>,
    Arc<Mutex<Option<NodeClose>>>,
) {
    let (client_end, node_end) = memory_pair(NonZeroUsize::new(16).unwrap());
    let closed = Arc::new(Mutex::new(None));
    let node_end = Closable {
        carrier: node_end,
        closed: Arc::clone(&closed),
    };
    let gateway = node.secure.clone();
    let accepted = tokio::spawn(async move { gateway.accept(node_end).await });
    (
        vessel_secure::initiate(client_end, key).await,
        accepted,
        closed,
    )
}

/// Pairing and authenticating through the real Noise channel, as `/secure`
/// and relays will carry it, with the client's own `secure::pair` and
/// `secure::open`.
#[tokio::test]
async fn devices_pair_and_authenticate_over_the_secure_channel() {
    let (_temp, node) = node().await;
    let offer = invite(&node, None);
    let config = ClientConfig::default();

    let (client_end, _closed) = carrier(&node);
    let (paired, connection) = secure::pair(client_end, &offer, "laptop", client(), &config)
        .await
        .unwrap();
    assert_eq!(paired.node.id, node.info().id);
    assert_eq!(connection.devices().await.unwrap().len(), 1);

    let (client_end, _closed) = carrier(&node);
    let connection = secure::open(
        client_end,
        &offer.key,
        offer.node,
        &paired.credential,
        client(),
        &config,
    )
    .await
    .unwrap();
    assert!(connection.sessions().await.unwrap().is_empty());

    // Spent: the same link again is refused, and that is final.
    let (client_end, _closed) = carrier(&node);
    let error = secure::pair(client_end, &offer, "again", client(), &config)
        .await
        .unwrap_err();
    assert!(error.is_final(), "{error}");
    node.stop().await.unwrap();
}

/// A client that pinned another key never gets a channel, and the node
/// closes the carrier with 4401 so the client knows it is final.
#[tokio::test]
async fn a_client_with_another_key_gets_no_channel() {
    let (_temp, node) = node().await;
    let other = *NodeKeypair::generate().unwrap().public();
    let (channel, accepted, closed) = secure(&node, &other).await;
    assert!(channel.is_err());
    assert!(matches!(
        accepted.await.unwrap(),
        Err(SecureError::Handshake(_))
    ));
    assert_eq!(*closed.lock().unwrap(), Some(NodeClose::HandshakeFailed));
    node.stop().await.unwrap();
}

#[tokio::test]
async fn pairing_creates_a_device_once_and_its_credential_authenticates_it() {
    let (_temp, node) = node().await;
    let offer = invite(&node, Some("work laptop"));
    let (device, secret, paired) = pair(&node, offer.clone()).await;

    // The channel that paired goes on to serve the protocol, with every scope.
    let devices: Vec<DeviceInfo> = paired.call(ListDevices).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id, device);
    assert_eq!(
        devices[0].label, "work laptop",
        "the invitation's label wins"
    );
    assert_eq!(devices[0].scopes, Scope::ALL.to_vec());

    // The invitation is spent.
    let mut again = channel(&node);
    assert_eq!(
        preamble(
            &mut again,
            &Preamble::PairRequest {
                invite: offer.invite,
                device_name: "intruder".to_owned()
            }
        )
        .await,
        PreambleReply::Refused {
            reason: RefusalReason::InvalidInvitation
        }
    );
    assert!(again.next().await.is_none(), "closed after refusing");

    // The credential works; a wrong one and an unknown device do not, and
    // are indistinguishable.
    let connection = authenticate(&node, device, &secret).await.unwrap();
    assert!(connection.call(ListSessions).await.unwrap().is_empty());
    let wrong = DeviceSecret::from_bytes([0; 32]);
    assert_eq!(
        authenticate(&node, device, &wrong).await.unwrap_err(),
        RefusalReason::UnknownDevice
    );
    assert_eq!(
        authenticate(&node, DeviceId::random(), &secret)
            .await
            .unwrap_err(),
        RefusalReason::UnknownDevice
    );
    node.stop().await.unwrap();
}

#[tokio::test]
async fn revoking_a_device_closes_its_connections_and_refuses_it_after() {
    let (_temp, node) = node().await;
    let (laptop, laptop_secret, laptop_paired) = pair(&node, invite(&node, None)).await;
    let (_phone, _phone_secret, phone) = pair(&node, invite(&node, None)).await;
    let laptop_again = authenticate(&node, laptop, &laptop_secret).await.unwrap();

    // The phone revokes the laptop.
    let _ack = phone.call(RevokeDevice { device: laptop }).await.unwrap();
    laptop_paired.closed().await;
    laptop_again.closed().await;
    assert_eq!(
        laptop_again.call(ListSessions).await,
        Err(CallError::Disconnected)
    );
    assert_eq!(
        authenticate(&node, laptop, &laptop_secret)
            .await
            .unwrap_err(),
        RefusalReason::Revoked
    );

    // The phone is unaffected, and still lists the laptop as revoked.
    let listed = phone.call(ListDevices).await.unwrap();
    let revoked = listed.iter().find(|device| device.id == laptop).unwrap();
    assert!(revoked.revoked_at.is_some());
    assert_eq!(
        phone
            .call(RevokeDevice {
                device: DeviceId::random()
            })
            .await
            .unwrap_err()
            .code(),
        Some(ErrorCode::NotFound)
    );
    node.stop().await.unwrap();
}

/// A device holds only its scopes; the check applies to every method.
#[tokio::test]
async fn a_device_without_a_scope_is_forbidden_that_method() {
    let (_temp, node) = node().await;
    let viewer = DeviceInfo {
        id: DeviceId::random(),
        label: "viewer".to_owned(),
        created_at: Utc::now(),
        last_seen: None,
        scopes: vec![Scope::Read],
        revoked_at: None,
    };
    let secret = DeviceSecret::from_bytes([9; 32]);
    node.secure
        .devices
        .store
        .add_device(viewer.clone(), CredentialHash::of(&secret))
        .await
        .unwrap();
    let connection = authenticate(&node, viewer.id, &secret).await.unwrap();
    assert!(connection.call(ListSessions).await.is_ok());
    let create = CreateSession {
        spec: SessionSpec {
            harness: HarnessKind::ClaudeCode,
            cwd: None,
            mode: SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            },
        },
    };
    for refused in [
        connection.call(create).await.unwrap_err(),
        connection.call(ListDevices).await.map(drop).unwrap_err(),
    ] {
        assert_eq!(refused.code(), Some(ErrorCode::Forbidden));
    }
    node.stop().await.unwrap();
}

/// A channel whose first frame is not a preamble gets no answer.
#[tokio::test]
async fn a_channel_without_a_preamble_is_dropped_unanswered() {
    let (_temp, node) = node().await;
    let channel = channel(&node);
    let refused = Connection::connect(channel, client(), &ClientConfig::default()).await;
    assert!(refused.is_err(), "{refused:?}");
    node.stop().await.unwrap();
}
