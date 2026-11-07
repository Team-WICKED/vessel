//! Reaching a paired node over a secure channel (ADR 0009, ADR 0010) on
//! any carrier: the Noise handshake to the pinned key, the device preamble,
//! then `hello` as on every transport.
//!
//! [Decision Log]
//! - Purpose: one place that turns "a carrier to some address" into a
//!   protocol connection with a paired node, the same way whether the
//!   carrier is a direct websocket, a relay's spliced connection, or an
//!   in-memory pair in a test.
//! - Choice: generic over `vessel_secure::Carrier`. Dialing routes (direct
//!   addresses first, then relays) belongs to [`SecureConnector`], which
//!   calls these with the carriers a [`Dialer`] opens (`vessel-relay`'s
//!   websocket dial, or in-memory carriers in tests).
//! - The key is checked by the handshake before anything is sent, so a
//!   credential never reaches anyone but the node pinned at pairing.
//! - Errors say whether trying again could help ([`SecureConnectError::
//!   is_final`]): a wrong key or tampering (`vessel-secure`'s verdicts), a
//!   revoked or unknown device, or a used invitation do not pass with time;
//!   a failed or slow network does.

mod connector;
mod dial;
mod route;
mod websocket;

use vessel_proto::{ClientInfo, NodeId, NodePublicKey, PairingOffer};
use vessel_rpc::ClientConfig;
use vessel_secure::{Carrier, SecureError};

pub use connector::{RouteTimeouts, SecureConnector, pair_over};
pub use dial::{Dialer, PeerClose};
pub use route::{ReachError, Route, RouteError, RouteFailure};
/// The websocket dial's errors and TLS setup, from `vessel-relay`, so callers
/// of [`WsDialer`] need no edge of their own to it.
pub use vessel_relay::{DialError, Keepalive, TlsError};
pub use websocket::WsDialer;

use crate::client::NodeClient;
use crate::credential::{CredentialMismatch, DeviceCredential};
use crate::pairing::{self, Paired, PreambleError};

/// Opening a secure connection failed.
#[derive(Debug, thiserror::Error)]
pub enum SecureConnectError {
    /// The credential belongs to another node; it was not sent, and no
    /// connection was attempted.
    #[error(transparent)]
    Credential(#[from] CredentialMismatch),
    /// The secure channel could not be set up: another key answered (the
    /// node was re-keyed, or it is another node), or the network failed.
    #[error("the secure connection failed")]
    Channel(#[source] SecureError),
    /// The node refused the device or the invitation, or did not answer.
    #[error(transparent)]
    Preamble(#[from] PreambleError),
    /// The protocol handshake after the preamble failed.
    #[error("the node refused the protocol handshake")]
    Handshake(#[source] vessel_rpc::ConnectError<SecureError>),
}

impl SecureConnectError {
    /// Whether another attempt cannot help (re-pair instead): a handshake
    /// verdict about the key or the path, a final refusal, or a hello the
    /// node rejected (no common protocol version). A network failure or
    /// timeout, and a node that cannot authenticate right now, may pass.
    ///
    /// NK gives a client that pinned the wrong key no answer: the node just
    /// closes, which looks here like a failed carrier (transient). Over a
    /// websocket the node closes with `NodeClose::HandshakeFailed` (4401);
    /// [`SecureConnector`] reads that code from the carrier's error through
    /// its [`Dialer`] and treats it as final.
    #[must_use]
    pub fn is_final(&self) -> bool {
        match self {
            Self::Credential(_) => true,
            Self::Channel(error) => !error.is_transient(),
            Self::Preamble(error) => error.is_final(),
            Self::Handshake(error) => match error {
                vessel_rpc::ConnectError::Transport(error) => !error.is_transient(),
                vessel_rpc::ConnectError::Timeout | vessel_rpc::ConnectError::Closed => false,
                vessel_rpc::ConnectError::Rejected(_)
                | vessel_rpc::ConnectError::Violation(_)
                | vessel_rpc::ConnectError::Decode(_)
                | vessel_rpc::ConnectError::Encode(_) => true,
            },
        }
    }

    /// Whether the node itself answered, after the handshake proved its
    /// key: a refusal of the device, or a rejected hello. Any route would
    /// get the same answer.
    #[must_use]
    pub const fn is_answer(&self) -> bool {
        match self {
            Self::Credential(_) | Self::Channel(_) => false,
            Self::Preamble(error) => error.is_answer(),
            Self::Handshake(error) => match error {
                vessel_rpc::ConnectError::Rejected(_) => true,
                vessel_rpc::ConnectError::Transport(_)
                | vessel_rpc::ConnectError::Timeout
                | vessel_rpc::ConnectError::Closed
                | vessel_rpc::ConnectError::Violation(_)
                | vessel_rpc::ConnectError::Decode(_)
                | vessel_rpc::ConnectError::Encode(_) => false,
            },
        }
    }
}

/// Open a protocol connection to the paired node `node` with `key` over
/// `carrier`, presenting `credential`, which must be that node's: it is
/// checked before the carrier is used.
///
/// # Errors
/// See [`SecureConnectError`].
pub async fn open<C: Carrier>(
    carrier: C,
    key: &NodePublicKey,
    node: NodeId,
    credential: &DeviceCredential,
    client: ClientInfo,
    config: &ClientConfig,
) -> Result<NodeClient, SecureConnectError> {
    credential.check(key, node)?;
    let channel = vessel_secure::initiate(carrier, key)
        .await
        .map_err(SecureConnectError::Channel)?;
    let channel = pairing::authenticate(channel, credential).await?;
    NodeClient::over(channel, client, config)
        .await
        .map_err(SecureConnectError::Handshake)
}

/// Pair with the node of `offer` over `carrier`, as `device_name`. Returns
/// the new credential and the node, and the connection that follows on the
/// same channel.
///
/// # Errors
/// See [`SecureConnectError`]; an invitation that is used, expired or for
/// another node is refused as final.
pub async fn pair<C: Carrier>(
    carrier: C,
    offer: &PairingOffer,
    device_name: &str,
    client: ClientInfo,
    config: &ClientConfig,
) -> Result<(Paired, NodeClient), SecureConnectError> {
    let channel = vessel_secure::initiate(carrier, &offer.key)
        .await
        .map_err(SecureConnectError::Channel)?;
    let (paired, channel) = pairing::pair(channel, offer, device_name).await?;
    let connection = NodeClient::over(channel, client, config)
        .await
        .map_err(SecureConnectError::Handshake)?;
    Ok((paired, connection))
}
