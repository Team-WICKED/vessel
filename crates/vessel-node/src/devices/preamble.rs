//! The device preamble (ADR 0010, PROTOCOL.md version 4): the first frame
//! a client sends on a secure channel, before `hello`, and the node's
//! answer.
//!
//! [Decision Log]
//! - Purpose: Noise NK authenticates the node, not the client. The client
//!   says who it is in this first encrypted frame: a device credential, or a
//!   pairing invitation.
//! - Choice: a function over any [`FrameTransport`], not over the secure
//!   channel type. It runs unchanged on `/secure`, on a relay's data
//!   connection, and in tests over an in-memory pair; the transport is
//!   trusted to be an established secure channel by whoever calls it.
//! - A refusal is an encrypted `refused` frame, then the channel closes:
//!   relays rewrite close codes, so the reason must travel inside (ADR 0009).
//!   A first frame that is not a preamble at all gets no answer; the peer is
//!   not speaking this protocol.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio_util::sync::CancellationToken;
use vessel_proto::{DeviceId, Preamble, PreambleReply, RefusalReason};
use vessel_rpc::FrameTransport;

use super::{Devices, Registration};
use crate::lifecycle::Lifecycle;
use crate::principal::Principal;

/// How long a new direct channel may take to send its preamble; a client
/// that connects and says nothing is dropped instead of holding a
/// connection. A relayed one has less (`Route::preamble_within`).
pub(crate) const PREAMBLE_TIMEOUT: Duration = Duration::from_secs(10);

/// A client the node will serve.
#[derive(Debug)]
pub(crate) struct Admitted {
    /// Who it is.
    pub(crate) principal: Principal,
    /// Its connection's registration (closed if the device is revoked).
    pub(crate) registration: Registration,
}

/// Why a channel was not admitted.
#[derive(Debug)]
pub(crate) enum NotAdmitted {
    /// Answered `refused` with this reason.
    Refused(RefusalReason),
    /// No preamble arrived (silence, end of stream, or another frame).
    NoPreamble(&'static str),
    /// The channel failed while answering.
    Transport(String),
}

/// Read the preamble on `channel` within `within` and answer it. On
/// success the caller serves the protocol on the same channel; on a
/// refusal the channel has been closed.
pub(crate) async fn admit<T: FrameTransport>(
    channel: &mut T,
    devices: &Devices,
    lifecycle: &Lifecycle,
    closing: &CancellationToken,
    within: Duration,
) -> Result<Admitted, NotAdmitted> {
    let first = match tokio::time::timeout(within, channel.next()).await {
        Err(_elapsed) => return Err(NotAdmitted::NoPreamble("no preamble in time")),
        Ok(None) => return Err(NotAdmitted::NoPreamble("closed before the preamble")),
        Ok(Some(Err(error))) => return Err(NotAdmitted::Transport(error.to_string())),
        Ok(Some(Ok(frame))) => frame,
    };
    let Ok(preamble) = serde_json::from_str::<Preamble>(&first) else {
        return Err(NotAdmitted::NoPreamble("the first frame is not a preamble"));
    };
    let outcome = match preamble {
        Preamble::DeviceAuth { device, secret } => {
            // Registered before the credential is checked; see `connected`.
            let registration = devices.connect(device, closing);
            devices.authenticate(device, &secret).await.map(|scopes| {
                let admitted = Admitted {
                    principal: Principal::Device { id: device, scopes },
                    registration,
                };
                (PreambleReply::Authenticated { device }, admitted)
            })
        }
        Preamble::PairRequest {
            invite,
            device_name,
        } => {
            // The new id is registered before the device is stored, as
            // above: a revocation that finds the device finds this too.
            let registration = devices.connect(DeviceId::random(), closing);
            let paired = devices.pair(&registration, &invite, &device_name).await;
            paired.map(|(secret, scopes)| {
                let device = registration.device();
                let admitted = Admitted {
                    principal: Principal::Device { id: device, scopes },
                    registration,
                };
                let reply = PreambleReply::Paired {
                    device,
                    secret,
                    node: Box::new(lifecycle.info()),
                };
                (reply, admitted)
            })
        }
    };
    match outcome {
        Ok((reply, admitted)) => {
            send(channel, &reply).await?;
            Ok(admitted)
        }
        Err(reason) => {
            tracing::info!(?reason, "refused a secure connection");
            send(channel, &PreambleReply::Refused { reason }).await?;
            if let Err(error) = channel.close().await {
                tracing::debug!(%error, "refused peer left before the close");
            }
            Err(NotAdmitted::Refused(reason))
        }
    }
}

async fn send<T: FrameTransport>(
    channel: &mut T,
    reply: &PreambleReply,
) -> Result<(), NotAdmitted> {
    let text =
        serde_json::to_string(reply).map_err(|error| NotAdmitted::Transport(error.to_string()))?;
    channel
        .send(text)
        .await
        .map_err(|error| NotAdmitted::Transport(error.to_string()))
}
