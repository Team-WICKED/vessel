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

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_relay::NodeCarrier;
use vessel_rpc::{FrameTransport, Server, ServerConfig};
use vessel_secure::{NodeKeypair, SecureError};

use crate::devices::Devices;
use crate::devices::preamble::{self, Admitted, NotAdmitted};
use crate::guard::Guarded;
use crate::lifecycle::Lifecycle;
use crate::service::Connection;

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
}

impl SecureGateway {
    /// Answer the Noise handshake on `carrier`, then serve the channel
    /// until it ends.
    ///
    /// # Errors
    /// The handshake failed (another key or version, a timeout, or the
    /// carrier); the carrier was closed with 4401 if it was the handshake
    /// itself.
    pub(crate) async fn accept<C: NodeCarrier>(&self, carrier: C) -> Result<(), SecureError> {
        let channel = vessel_relay::respond(carrier, &self.keypair, &self.channel).await?;
        self.serve(channel).await;
        Ok(())
    }

    /// [`accept`](Self::accept) for a carrier from a transport task: the
    /// failure is logged, since nobody waits for it.
    pub(crate) async fn accept_logged<C: NodeCarrier>(&self, carrier: C, via: &'static str) {
        if let Err(error) = self.accept(carrier).await {
            tracing::info!(via, error = %error, "a secure handshake failed");
        }
    }

    /// Serve one established secure channel until it ends: the preamble,
    /// then the protocol as the admitted device.
    pub(crate) async fn serve<T: FrameTransport>(&self, mut channel: T) {
        let _tracked = self.connections.token();
        let admitted =
            preamble::admit(&mut channel, &self.devices, &self.lifecycle, &self.closing).await;
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
