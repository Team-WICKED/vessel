//! Serving paired devices over a secure channel (ADR 0009, ADR 0010).
//!
//! A carrier arrives here from `/secure` on the node's listener, or as a
//! relay's data connection. The gateway answers the Noise handshake with
//! the node's key ([`SecureGateway::accept`]), runs the device preamble,
//! then the protocol, with the device as the connection's principal.
//!
//! [Decision Log]
//! - Per-connection server: each device connection gets its own
//!   `vessel-rpc` `Server` (cheap: shared handler, a watch receiver, a
//!   config) whose shutdown token is the connection's registration, a child
//!   of the node's closing token. Revoking the device cancels it, and the
//!   RPC layer closes the connection in its usual order. The alternative,
//!   one shared server, has no way to close one connection from outside.
//! - The gateway is generic over the carrier and the channel, so the same
//!   code serves `/secure` ([`vessel_relay::AxumCarrier`]), relay data
//!   connections ([`vessel_relay::WsCarrier`]) and the in-memory pairs of
//!   the tests.
//! - The handshake goes through [`vessel_relay::respond`], which closes the
//!   carrier with `NodeClose::HandshakeFailed` (4401) when it fails: with
//!   NK only the node can tell that a client pinned another key, and the
//!   code is how the client learns it (final: pair again).
//! - Unadmitted channels are capped ([`MAX_UNADMITTED`]): anyone who can
//!   reach `/secure` or a relay can open channels, and each one may sit
//!   in the handshake (10 s) and the preamble (10 s) before it is known
//!   to be a device. A permit is taken before the handshake and given
//!   back once the preamble is answered; past the cap a new carrier is
//!   dropped at once, which a client sees as a transient failure. Admitted
//!   connections are not counted: they belong to devices, which can be
//!   revoked. The alternative, a cap per source address, has no address to
//!   go by behind a relay.
//! - Until the preamble is answered, frames are limited to
//!   [`PREAMBLE_MAX_FRAME`] (64 KiB) both ways: a preamble is a few hundred
//!   bytes, and a stranger must not make the node buffer 16 MiB per
//!   channel. Admission raises the limit to the channel's configured one
//!   (`SecureChannel::set_max_frame`); a frame the client pipelined behind
//!   its preamble is judged when it is read, under the raised limit.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_relay::NodeCarrier;
use vessel_rpc::{FrameTransport, Server, ServerConfig};
use vessel_secure::{Carrier, NodeKeypair, SecureChannel, SecureError};

use crate::devices::Devices;
use crate::devices::preamble::{self, Admitted, NotAdmitted};
use crate::guard::Guarded;
use crate::lifecycle::Lifecycle;
use crate::service::Connection;

/// How many secure channels may be in their handshake or preamble at once.
pub(crate) const MAX_UNADMITTED: usize = 64;

/// The largest frame either side may send before the client is admitted.
pub(crate) const PREAMBLE_MAX_FRAME: u32 = 64 * 1024;

/// A channel whose frame limit is raised once its client is admitted.
pub(crate) trait Admittable: FrameTransport {
    /// Allow frames of up to `max_frame` bytes from now on.
    fn admitted(&mut self, max_frame: u32);
}

impl<C: Carrier> Admittable for SecureChannel<C> {
    fn admitted(&mut self, max_frame: u32) {
        self.set_max_frame(max_frame);
    }
}

/// The in-memory frames the device tests use have no size limit to raise.
#[cfg(test)]
impl Admittable for vessel_rpc::memory::MemoryTransport {
    fn admitted(&mut self, _max_frame: u32) {}
}

/// Why a carrier was not served.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NotServed {
    /// Too many channels are waiting to be admitted; dropped at once.
    #[error("too many secure connections are waiting to be admitted")]
    Busy,
    /// The handshake failed; the carrier was closed with 4401 if it was
    /// the handshake itself.
    #[error(transparent)]
    Handshake(#[from] SecureError),
}

/// What serving a device connection needs.
#[derive(Debug, Clone)]
pub(crate) struct SecureGateway {
    /// The node's key pair, for the handshake.
    pub(crate) keypair: Arc<NodeKeypair>,
    /// The channel's limits (frame size, handshake timeout).
    pub(crate) channel: vessel_secure::Config,
    pub(crate) service: Arc<Guarded>,
    pub(crate) lifecycle: Lifecycle,
    pub(crate) devices: Arc<Devices>,
    pub(crate) config: ServerConfig,
    /// The node's connection shutdown; each connection's token is a child.
    pub(crate) closing: CancellationToken,
    /// Tracked so shutdown waits for these connections too.
    pub(crate) connections: TaskTracker,
    /// Permits for channels not yet admitted ([`MAX_UNADMITTED`]).
    pub(crate) unadmitted: Arc<Semaphore>,
}

impl SecureGateway {
    /// Answer the Noise handshake on `carrier`, then serve the channel
    /// until it ends.
    ///
    /// # Errors
    /// Too many channels are waiting to be admitted, or the handshake
    /// failed (another key or version, a timeout, or the carrier).
    pub(crate) async fn accept<C: NodeCarrier>(&self, carrier: C) -> Result<(), NotServed> {
        let permit = self.permit().ok_or(NotServed::Busy)?;
        let mut unadmitted = self.channel;
        unadmitted.max_frame = PREAMBLE_MAX_FRAME.min(self.channel.max_frame);
        let channel = vessel_relay::respond(carrier, &self.keypair, &unadmitted).await?;
        self.serve_with(channel, permit).await;
        Ok(())
    }

    fn permit(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.unadmitted).try_acquire_owned().ok()
    }

    /// [`accept`](Self::accept) for a carrier from a transport task: the
    /// failure is logged, since nobody waits for it.
    pub(crate) async fn accept_logged<C: NodeCarrier>(&self, carrier: C, via: &'static str) {
        if let Err(error) = self.accept(carrier).await {
            tracing::info!(via, error = %error, "a secure handshake failed");
        }
    }

    /// Serve one established secure channel until it ends: the preamble,
    /// then the protocol as the admitted device. Dropped at once past
    /// [`MAX_UNADMITTED`].
    #[cfg(test)]
    pub(crate) async fn serve<T: Admittable>(&self, channel: T) {
        if let Some(permit) = self.permit() {
            self.serve_with(channel, permit).await;
        } else {
            tracing::info!("dropped a secure connection: too many waiting");
        }
    }

    async fn serve_with<T: Admittable>(&self, mut channel: T, permit: OwnedSemaphorePermit) {
        let _tracked = self.connections.token();
        // Shutdown does not wait out a silent client's preamble timeout.
        let admitted = tokio::select! {
            admitted = preamble::admit(&mut channel, &self.devices, &self.lifecycle, &self.closing) => admitted,
            () = self.closing.cancelled() => return,
        };
        drop(permit);
        let Admitted {
            principal,
            registration,
        } = match admitted {
            Ok(admitted) => admitted,
            Err(NotAdmitted::Refused(_reason)) => return,
            Err(NotAdmitted::NoPreamble(why)) => {
                tracing::info!(why, "dropped a secure connection without a preamble");
                return;
            }
            Err(NotAdmitted::Transport(error)) => {
                tracing::info!(%error, "a secure connection failed during its preamble");
                return;
            }
        };
        channel.admitted(self.channel.max_frame);
        let server = Server::new(
            Arc::clone(&self.service),
            self.lifecycle.watch_info(),
            self.config.clone(),
            registration.closing().clone(),
        );
        if let Err(error) = server
            .serve(channel, move |peer| Connection::new(peer, principal))
            .await
        {
            tracing::info!(%error, "a device connection ended abnormally");
        }
        drop(registration);
    }
}
