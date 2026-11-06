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
//!   addresses first, then relays) belongs to the connector, which comes
//!   with `vessel-relay`'s websocket carrier and dial; it calls these.
//! - The key is checked by the handshake before anything is sent, so a
//!   credential never reaches anyone but the node pinned at pairing.
//! - Errors say whether trying again could help ([`SecureConnectError::
//!   is_final`]): a wrong key or tampering (`vessel-secure`'s verdicts), a
//!   revoked or unknown device, or a used invitation do not pass with time;
//!   a failed or slow network does.

use vessel_proto::{ClientInfo, NodePublicKey, PairingOffer};
use vessel_rpc::ClientConfig;
use vessel_secure::{Carrier, SecureError};

use crate::client::NodeClient;
use crate::credential::DeviceCredential;
use crate::pairing::{self, Paired, PreambleError};

/// Opening a secure connection failed.
#[derive(Debug, thiserror::Error)]
pub enum SecureConnectError {
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
    /// verdict about the key or the path, or a final refusal. A network
    /// failure or timeout, and a node that cannot authenticate right now,
    /// may pass.
    ///
    /// NK gives a client that pinned the wrong key no answer: the node just
    /// closes, which looks here like a failed carrier (transient). Over a
    /// websocket the node closes with `NodeClose::HandshakeFailed` (4401);
    /// the connector reads that code from the carrier's error and treats
    /// it as final.
    #[must_use]
    pub fn is_final(&self) -> bool {
        match self {
            Self::Channel(error) => !error.is_transient(),
            Self::Preamble(error) => error.is_final(),
            Self::Handshake(_) => false,
        }
    }
}

/// Open a protocol connection to the paired node with `key` over `carrier`,
/// presenting `credential`.
///
/// # Errors
/// See [`SecureConnectError`].
pub async fn open<C: Carrier>(
    carrier: C,
    key: &NodePublicKey,
    credential: &DeviceCredential,
    client: ClientInfo,
    config: &ClientConfig,
) -> Result<NodeClient, SecureConnectError> {
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
    let (paired, channel) = pairing::pair(channel, &offer.invite, device_name).await?;
    let connection = NodeClient::over(channel, client, config)
        .await
        .map_err(SecureConnectError::Handshake)?;
    Ok((paired, connection))
}
