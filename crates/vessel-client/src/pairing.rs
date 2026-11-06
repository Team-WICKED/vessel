//! The device preamble from the client's side (ADR 0010): pairing with an
//! invitation, or authenticating with a credential, as the first frame on a
//! secure channel, before `hello`.
//!
//! [Decision Log]
//! - Purpose: the node knows who a secure channel belongs to only from this
//!   frame (Noise NK authenticates the node, not the client).
//! - Choice: functions over any [`FrameTransport`] that hand the channel
//!   back after a successful answer, so `hello` follows on it through
//!   [`NodeClient::over`](crate::NodeClient::over). They run unchanged on
//!   `vessel-secure`'s channel, direct or relayed, and in tests over an
//!   in-memory pair. The caller has already run the handshake to the pinned
//!   key: a credential is only ever sent inside such a channel.
//! - Refusals are typed ([`PreambleError::Refused`]) and say whether trying
//!   again could help ([`PreambleError::is_final`]): a revoked device or a
//!   used invitation stays refused, so a reconnecting stream must stop
//!   rather than retry (ADR 0010).

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use vessel_proto::{
    DeviceId, DeviceSecret, InvitationSecret, NodeInfo, Preamble, PreambleReply, RefusalReason,
};
use vessel_rpc::FrameTransport;

use crate::credential::DeviceCredential;

/// How long the node may take to answer the preamble.
pub const PREAMBLE_TIMEOUT: Duration = Duration::from_secs(10);

/// A device the node just paired.
#[derive(Debug, Clone)]
pub struct Paired {
    /// The node, whose id the client pins.
    pub node: NodeInfo,
    /// The new device's credential, to store (`DeviceCredential::write`).
    pub credential: DeviceCredential,
}

/// The preamble failed.
#[derive(Debug, thiserror::Error)]
pub enum PreambleError {
    /// The node said no, and why.
    #[error("{}", refusal(*.0))]
    Refused(RefusalReason),
    /// The channel ended before the node answered.
    #[error("the node closed the connection before answering")]
    Closed,
    /// The node did not answer in time.
    #[error("the node did not answer within {} s", PREAMBLE_TIMEOUT.as_secs())]
    Timeout,
    /// The node answered something else.
    #[error("the node's answer is not a preamble reply: {0}")]
    Protocol(String),
    /// The channel failed.
    #[error("the connection failed: {0}")]
    Transport(String),
}

impl PreambleError {
    /// Whether trying again cannot help: an unknown or revoked device, or
    /// an invalid invitation. Everything else may be passing.
    #[must_use]
    pub const fn is_final(&self) -> bool {
        match self {
            Self::Refused(reason) => match reason {
                RefusalReason::UnknownDevice
                | RefusalReason::Revoked
                | RefusalReason::InvalidInvitation => true,
                RefusalReason::Unavailable => false,
            },
            Self::Closed | Self::Timeout | Self::Protocol(_) | Self::Transport(_) => false,
        }
    }
}

const fn refusal(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::UnknownDevice => {
            "the node does not know this device (removed, or the credential is wrong); pair it again"
        }
        RefusalReason::Revoked => {
            "this device was revoked on the node; pair it again if that was a mistake"
        }
        RefusalReason::InvalidInvitation => {
            "this invitation is not valid (used, expired, or for another node); make a new one \
             with `vessel node pair`"
        }
        RefusalReason::Unavailable => {
            "the node cannot authenticate devices right now; try again shortly"
        }
    }
}

/// Redeem `invite` as a device called `device_name`. On success the channel
/// is returned for `hello`, and the result holds the new credential.
///
/// # Errors
/// The node refused, or the channel failed or timed out.
pub async fn pair<T: FrameTransport>(
    mut channel: T,
    invite: &InvitationSecret,
    device_name: &str,
) -> Result<(Paired, T), PreambleError> {
    let request = Preamble::PairRequest {
        invite: invite.clone(),
        device_name: device_name.to_owned(),
    };
    match exchange(&mut channel, &request).await? {
        PreambleReply::Paired {
            device,
            secret,
            node,
        } => Ok((
            Paired {
                node: *node,
                credential: DeviceCredential { device, secret },
            },
            channel,
        )),
        PreambleReply::Refused { reason } => Err(PreambleError::Refused(reason)),
        PreambleReply::Authenticated { .. } => Err(PreambleError::Protocol(
            "`authenticated` in answer to a pairing request".to_owned(),
        )),
    }
}

/// Present `credential`. On success the channel is returned for `hello`.
///
/// # Errors
/// The node refused, or the channel failed or timed out.
pub async fn authenticate<T: FrameTransport>(
    mut channel: T,
    credential: &DeviceCredential,
) -> Result<T, PreambleError> {
    let request = Preamble::DeviceAuth {
        device: credential.device,
        secret: credential.secret.clone(),
    };
    match exchange(&mut channel, &request).await? {
        PreambleReply::Authenticated { device } if device == credential.device => Ok(channel),
        PreambleReply::Authenticated { device } => Err(PreambleError::Protocol(format!(
            "authenticated as device {device}, not {}",
            credential.device
        ))),
        PreambleReply::Refused { reason } => Err(PreambleError::Refused(reason)),
        PreambleReply::Paired { .. } => Err(PreambleError::Protocol(
            "`paired` in answer to a device credential".to_owned(),
        )),
    }
}

async fn exchange<T: FrameTransport>(
    channel: &mut T,
    request: &Preamble,
) -> Result<PreambleReply, PreambleError> {
    let text = serde_json::to_string(request)
        .map_err(|error| PreambleError::Protocol(error.to_string()))?;
    channel
        .send(text)
        .await
        .map_err(|error| PreambleError::Transport(error.to_string()))?;
    let answer = tokio::time::timeout(PREAMBLE_TIMEOUT, channel.next())
        .await
        .map_err(|_elapsed| PreambleError::Timeout)?
        .ok_or(PreambleError::Closed)?
        .map_err(|error| PreambleError::Transport(error.to_string()))?;
    serde_json::from_str(&answer).map_err(|error| PreambleError::Protocol(error.to_string()))
}

/// A device's credential and id, as [`authenticate`] needs them; for
/// callers that hold the parts.
#[must_use]
pub const fn credential(device: DeviceId, secret: DeviceSecret) -> DeviceCredential {
    DeviceCredential { device, secret }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use vessel_rpc::memory;

    use super::*;

    /// A node that answers one preamble with `reply`.
    fn node_answering(reply: PreambleReply) -> memory::MemoryTransport {
        let (client, mut node) = memory::pair(NonZeroUsize::new(4).unwrap());
        drop(tokio::spawn(async move {
            let _preamble = node.next().await;
            node.send(serde_json::to_string(&reply).unwrap())
                .await
                .unwrap();
        }));
        client
    }

    #[tokio::test]
    async fn refusals_are_typed_and_say_whether_to_retry() {
        let credential = credential(DeviceId::random(), DeviceSecret::from_bytes([1; 32]));
        for (reason, final_) in [
            (RefusalReason::Revoked, true),
            (RefusalReason::UnknownDevice, true),
            (RefusalReason::Unavailable, false),
        ] {
            let channel = node_answering(PreambleReply::Refused { reason });
            let error = authenticate(channel, &credential).await.unwrap_err();
            assert!(matches!(error, PreambleError::Refused(got) if got == reason));
            assert_eq!(error.is_final(), final_, "{reason:?}");
        }
        let channel = node_answering(PreambleReply::Authenticated {
            device: DeviceId::random(),
        });
        assert!(matches!(
            authenticate(channel, &credential).await,
            Err(PreambleError::Protocol(_))
        ));
        let (closed, node) = memory::pair(NonZeroUsize::new(4).unwrap());
        drop(node);
        assert!(
            !authenticate(closed, &credential)
                .await
                .unwrap_err()
                .is_final()
        );
    }
}
