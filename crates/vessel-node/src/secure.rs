//! Serving paired devices over a secure channel (ADR 0009, ADR 0010).
//!
//! A channel arrives here already through its Noise handshake: from
//! `/secure` on the node's listener, or from a relay's data connection. The
//! gateway runs the device preamble, then the protocol, with the device as
//! the connection's principal.
//!
//! [Decision Log]
//! - Per-connection server: each device connection gets its own
//!   `vessel-rpc` `Server` (cheap: shared handler, a watch receiver, a
//!   config) whose shutdown token is the connection's registration, a child
//!   of the node's closing token. Revoking the device cancels it, and the
//!   RPC layer closes the connection in its usual order. The alternative,
//!   one shared server, has no way to close one connection from outside.
//! - The gateway is generic over the transport, so it is tested now over
//!   in-memory pairs and takes `vessel-secure`'s channel unchanged later.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "reached through /secure and relay data connections once vessel-secure lands"
    )
)]

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_rpc::{FrameTransport, Server, ServerConfig};

use crate::devices::Devices;
use crate::devices::preamble::{self, Admitted, NotAdmitted};
use crate::guard::Guarded;
use crate::lifecycle::Lifecycle;
use crate::service::Connection;

/// What serving a device connection needs.
#[derive(Debug, Clone)]
pub(crate) struct SecureGateway {
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
    /// Serve one secure channel until it ends: the preamble, then the
    /// protocol as the admitted device.
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
