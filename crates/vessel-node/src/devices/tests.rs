//! Devices end to end inside the node: invitations, the preamble on a
//! channel (an in-memory pair standing in for a secure channel), the scope
//! check, and revocation closing connections.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use futures_util::{SinkExt as _, StreamExt as _};
use tempfile::TempDir;
use vessel_harness::Lookup;
use vessel_proto::{
    ClientInfo, CreateSession, DeviceId, DeviceInfo, DeviceSecret, ErrorCode, HarnessKind,
    InvitationSecret, InviteDevice, ListDevices, ListSessions, NodePublicKey, PairingOffer,
    Preamble, PreambleReply, RefusalReason, RevokeDevice, Scope, SessionMode, SessionSpec,
    TerminalSize,
};
use vessel_rpc::memory::{self, MemoryTransport};
use vessel_rpc::{CallError, ClientConfig, Connection};
use vessel_store::{CredentialHash, DataDir};

use super::*;
use crate::{Config, Node};

const KEY: NodePublicKey = NodePublicKey::from_bytes([7; 32]);

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
    node.secure.devices.set_key(KEY);
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

/// Pair through a fresh channel; returns the device, its secret, and the
/// protocol connection that follows on the same channel.
async fn pair(node: &Node, invite: InvitationSecret) -> (DeviceId, DeviceSecret, Connection) {
    let mut channel = channel(node);
    let reply = preamble(
        &mut channel,
        &Preamble::PairRequest {
            invite,
            device_name: "laptop".to_owned(),
        },
    )
    .await;
    let PreambleReply::Paired {
        device,
        secret,
        node: info,
    } = reply
    else {
        panic!("expected paired, got {reply:?}");
    };
    assert_eq!(info.id, node.info().id);
    let connection = Connection::connect(channel, client(), &ClientConfig::default())
        .await
        .unwrap();
    (device, secret, connection)
}

/// Authenticate through a fresh channel.
async fn authenticate(
    node: &Node,
    device: DeviceId,
    secret: &DeviceSecret,
) -> Result<Connection, RefusalReason> {
    let mut channel = channel(node);
    let reply = preamble(
        &mut channel,
        &Preamble::DeviceAuth {
            device,
            secret: secret.clone(),
        },
    )
    .await;
    match reply {
        PreambleReply::Authenticated { device: answered } => {
            assert_eq!(answered, device);
            Ok(
                Connection::connect(channel, client(), &ClientConfig::default())
                    .await
                    .unwrap(),
            )
        }
        PreambleReply::Refused { reason } => {
            assert!(channel.next().await.is_none(), "closed after refusing");
            Err(reason)
        }
        PreambleReply::Paired { .. } => panic!("paired on a device auth"),
    }
}

#[tokio::test]
async fn an_invitation_is_a_link_to_this_node_with_its_routes() {
    let (_temp, node) = node().await;
    let offer = invite(&node, Some("phone"));
    assert_eq!(offer.node, node.info().id);
    assert_eq!(offer.key, KEY);
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

#[tokio::test]
async fn without_a_key_there_is_no_link() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config::new(DataDir::new(temp.path()));
    config.bind = None;
    let node = Node::start(config).await.unwrap();
    assert!(node.info().public_key.is_none());
    assert!(matches!(
        node.secure.devices.invite(&InviteDevice {
            label: None,
            ttl_secs: 60
        }),
        Err(ServiceError::NoNodeKey)
    ));
    assert!(
        DataDir::new(temp.path()).read_node_key().is_ok(),
        "created on start"
    );
    node.stop().await.unwrap();
}

#[tokio::test]
async fn pairing_creates_a_device_once_and_its_credential_authenticates_it() {
    let (_temp, node) = node().await;
    let offer = invite(&node, Some("work laptop"));
    let (device, secret, paired) = pair(&node, offer.invite.clone()).await;

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
    let (laptop, laptop_secret, laptop_paired) = pair(&node, invite(&node, None).invite).await;
    let (_phone, _phone_secret, phone) = pair(&node, invite(&node, None).invite).await;
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
