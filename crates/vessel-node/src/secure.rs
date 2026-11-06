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
//! - Relayed channels wait in a budget of their own
//!   ([`vessel_relay::RELAYED_UNADMITTED`], 40), apart from the direct
//!   route's ([`MAX_UNADMITTED`], 64), and have
//!   [`vessel_relay::RELAYED_ADMISSION`] (5 s) for the handshake and the
//!   preamble together, not 10 s each (the security review, round 9). One
//!   address block's clients through a relay held 64 handshakes open
//!   without a preamble and so took every permit: no device could connect
//!   by any route. Now a crowd through a relay takes only relayed room,
//!   the relay lets one block hold few of it (`max_node_clients_per_address`),
//!   and the short deadline is what lets a relay tell an admitted client
//!   from one held open (it counts a splice open past it, and a margin,
//!   as admitted). Alternatives: one budget with a share kept for direct
//!   channels (a direct crowd would still take relayed room); a budget
//!   per relay (one relay's crowd would still fill it). Cost: a device
//!   behind a relay over a link slower than a few seconds a round trip
//!   cannot connect through it.
//! - Round 10: the relayed budget is each relay link's own (made with the
//!   link, gone with it), not one for every relay: with three relays one
//!   `/48` held all 40 places through them (16, 16 and 8), and a member
//!   whose only routes are its home's relays (up to 8 of them) could not
//!   be reached at all. Links are few (the configured relays and at most
//!   8 a home names), so the total stays bounded, and each relay's own
//!   startup check (a `/48`'s share under half of 40) holds per link. The
//!   direct budget is held per address too, [`MAX_UNADMITTED_PER_ADDRESS`]
//!   for an IPv4 address or an IPv6 `/64`, scaled for `/56` and `/48` as a
//!   relay counts (`vessel_relay::address`, shared): one host held all 64
//!   direct places for 20 s at a time. The address is the socket's peer;
//!   the node has no proxy mode, so there is no header to believe.
//! - Until the preamble is answered, frames are limited to
//!   [`PREAMBLE_MAX_FRAME`] (64 KiB) both ways: a preamble is a few hundred
//!   bytes, and a stranger must not make the node buffer 16 MiB per
//!   channel. Admission raises the limit to the channel's configured one
//!   (`SecureChannel::set_max_frame`); a frame the client pipelined behind
//!   its preamble is judged when it is read, under the raised limit.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_relay::NodeCarrier;
use vessel_relay::address::{Block, BlockCounts, blocks};
use vessel_rpc::{FrameTransport, Server, ServerConfig};
use vessel_secure::{Carrier, NodeKeypair, SecureChannel, SecureError};

use crate::devices::Devices;
use crate::devices::preamble::{self, Admitted, NotAdmitted, PREAMBLE_TIMEOUT};
use crate::error::chain;
use crate::guard::Guarded;
use crate::lifecycle::Lifecycle;
use crate::service::Connection;

/// How many secure channels from the direct route may be in their
/// handshake or preamble at once (each relay link has a budget of its own,
/// [`vessel_relay::RELAYED_UNADMITTED`]).
pub(crate) const MAX_UNADMITTED: usize = 64;

/// Of [`MAX_UNADMITTED`], how many one address may hold: an IPv4 address
/// or an IPv6 `/64`; a `/56` twice this and a `/48` four times
/// (`vessel_relay::address::WIDEST`), under half of [`MAX_UNADMITTED`] (a
/// test holds it to that, as a relay's startup check holds its limits).
pub(crate) const MAX_UNADMITTED_PER_ADDRESS: usize = 7;

/// The places of channels waiting for admission through one relay link.
pub(crate) type Budget = Arc<Semaphore>;

/// A new budget for one relay link ([`vessel_relay::RELAYED_UNADMITTED`]).
pub(crate) fn relay_budget() -> Budget {
    Arc::new(Semaphore::new(vessel_relay::RELAYED_UNADMITTED))
}

/// Which route a carrier came by, and where it waits for admission.
#[derive(Debug, Clone)]
pub(crate) enum Route {
    /// `/secure` on the node's own listener, from this peer.
    Direct(IpAddr),
    /// A relay's data connection, waiting in its link's own budget.
    Relayed(Budget),
}

impl Route {
    /// The route's name in the log.
    const fn label(&self) -> &'static str {
        match self {
            Self::Direct(_) => "direct",
            Self::Relayed(_) => "relay",
        }
    }

    /// How long a channel by this route has for its preamble, once its
    /// handshake took `handshake`.
    fn preamble_within(&self, handshake: Duration) -> Duration {
        match self {
            Self::Direct(_) => PREAMBLE_TIMEOUT,
            Self::Relayed(_) => vessel_relay::RELAYED_ADMISSION.saturating_sub(handshake),
        }
    }
}

/// A channel's places while it waits for admission, given back when it is
/// admitted or gone.
struct Waiting {
    _permit: OwnedSemaphorePermit,
    _address: Option<AddressPlace>,
}

/// A direct channel's place for its address, given back on drop.
struct AddressPlace {
    counts: Arc<Mutex<BlockCounts>>,
    blocks: Vec<Block>,
}

impl Drop for AddressPlace {
    fn drop(&mut self) {
        lock(&self.counts).remove(&self.blocks);
    }
}

fn lock(counts: &Mutex<BlockCounts>) -> MutexGuard<'_, BlockCounts> {
    // A few map operations that cannot panic halfway: a poisoned lock
    // still holds consistent counts.
    counts.lock().unwrap_or_else(PoisonError::into_inner)
}

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
    /// Permits for direct channels not yet admitted ([`MAX_UNADMITTED`]).
    pub(crate) unadmitted: Arc<Semaphore>,
    /// Direct channels not yet admitted, per address block
    /// ([`MAX_UNADMITTED_PER_ADDRESS`]).
    pub(crate) unadmitted_per_address: Arc<Mutex<BlockCounts>>,
}

impl SecureGateway {
    /// Answer the Noise handshake on `carrier`, which came by `route`,
    /// then serve the channel until it ends.
    ///
    /// # Errors
    /// Too many channels are waiting to be admitted by that route, or the
    /// handshake failed (another key or version, a timeout, or the
    /// carrier).
    pub(crate) async fn accept<C: NodeCarrier>(
        &self,
        carrier: C,
        route: Route,
    ) -> Result<(), NotServed> {
        let waiting = self.waiting(&route).ok_or(NotServed::Busy)?;
        let started = Instant::now();
        let mut unadmitted = self.channel;
        unadmitted.max_frame = PREAMBLE_MAX_FRAME.min(self.channel.max_frame);
        if matches!(route, Route::Relayed(_)) {
            unadmitted.handshake_timeout = unadmitted
                .handshake_timeout
                .min(vessel_relay::RELAYED_ADMISSION);
        }
        let channel = vessel_relay::respond(carrier, &self.keypair, &unadmitted).await?;
        let within = route.preamble_within(started.elapsed());
        self.serve_with(channel, waiting, within).await;
        Ok(())
    }

    /// A place to wait for admission by `route`: in the direct budget and
    /// for the peer's address blocks, or in the relay link's budget.
    fn waiting(&self, route: &Route) -> Option<Waiting> {
        match route {
            Route::Direct(peer) => {
                let blocks = blocks(*peer);
                let mut counts = lock(&self.unadmitted_per_address);
                if !counts.fits(&blocks, MAX_UNADMITTED_PER_ADDRESS) {
                    return None;
                }
                let permit = Arc::clone(&self.unadmitted).try_acquire_owned().ok()?;
                counts.add(&blocks);
                drop(counts);
                Some(Waiting {
                    _permit: permit,
                    _address: Some(AddressPlace {
                        counts: Arc::clone(&self.unadmitted_per_address),
                        blocks,
                    }),
                })
            }
            Route::Relayed(budget) => Some(Waiting {
                _permit: Arc::clone(budget).try_acquire_owned().ok()?,
                _address: None,
            }),
        }
    }

    /// [`accept`](Self::accept) for a carrier from a transport task: the
    /// failure is logged, since nobody waits for it.
    pub(crate) async fn accept_logged<C: NodeCarrier>(&self, carrier: C, route: Route) {
        let via = route.label();
        if let Err(error) = self.accept(carrier, route).await {
            tracing::info!(via, error = %chain(&error), "a secure handshake failed");
        }
    }

    /// Serve one established secure channel until it ends: the preamble,
    /// then the protocol as the admitted device. Dropped at once past
    /// [`MAX_UNADMITTED`].
    /// It came by `route`.
    #[cfg(test)]
    pub(crate) async fn serve<T: Admittable>(&self, channel: T, route: Route) {
        if let Some(waiting) = self.waiting(&route) {
            let within = route.preamble_within(Duration::ZERO);
            self.serve_with(channel, waiting, within).await;
        } else {
            tracing::info!(
                via = route.label(),
                "dropped a secure connection: too many waiting"
            );
        }
    }

    async fn serve_with<T: Admittable>(
        &self,
        mut channel: T,
        waiting: Waiting,
        preamble_within: Duration,
    ) {
        let _tracked = self.connections.token();
        // Shutdown does not wait out a silent client's preamble timeout.
        let admitted = tokio::select! {
            admitted = preamble::admit(&mut channel, &self.devices, &self.lifecycle, &self.closing, preamble_within) => admitted,
            () = self.closing.cancelled() => return,
        };
        drop(waiting);
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
            tracing::info!(error = %chain(&error), "a device connection ended abnormally");
        }
        drop(registration);
    }
}
