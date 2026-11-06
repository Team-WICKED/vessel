//! `fleet.callback` (protocol 6): how a member with no route of its own
//! (no `--relay`, no `--public`: plain `vessel node start`, or the service
//! `vessel init --service` installs) is called back by its home: through
//! the home's relays, registered with a ticket for its own key.
//!
//! [Decision Log]
//! - Purpose: the home reaches a member through the routes its grant's
//!   link names; a member started plainly named none, so the home refused
//!   its grant and never reached it (Track S's fresh-machine smoke).
//! - First built (103d765) by handing members the home's relay token. The
//!   review's round 6: any `sync` device got it (a stranger, a forgotten
//!   node), it kept working after the device was revoked, it registered any
//!   key (a free general relay), members never refreshed it (a rotated
//!   token, or a member moving homes, silently broke callbacks), and
//!   members dialed whatever URL the home named.
//! - Choice, the home's side: a ticket (`vessel_relay::issue_ticket`) for
//!   the asking member's own key, never the token. Only a device the
//!   home's record binds to that node and key, holding a current grant,
//!   gets a day's ticket ([`TICKET_LIFETIME`]). Before its first grant a
//!   device gets a short one ([`FIRST_TICKET_LIFETIME`], enough to register
//!   and grant) only for a claim the record would take, and for one key
//!   only (kept in memory: a home's restart allows one more). A forgotten
//!   node's old devices, devices bound elsewhere and revoked devices (which
//!   cannot connect) get none; a ticket already given lapses within its
//!   lifetime, and rotating the token voids them all.
//! - Choice, the member's side: it asks on every connection to its home,
//!   then again halfway to the ticket's expiry while connected
//!   (`keep_callback`); the relay links take each newer ticket and register
//!   again with it. The links belong to the current home: another home's
//!   callback, or leaving (`reconcile`), stops them, and only the grant's
//!   link names them, never the node's own pairing links.
//! - Which relays a member dials: at most [`MAX_CALLBACK_RELAYS`], each
//!   `wss://` to a public host, or any on a host the member reaches its
//!   home by (its membership's routes: the same machine or network, as the
//!   tests and the smoke run). No opt-in flag: a home on the same network
//!   names its relay by that network's address, which its members already
//!   reach it by. Others are logged and left out, so the home cannot point
//!   members at their own loopback or private network.
//! - Alternatives: the token (above); the home calling members back over
//!   the connection they keep to it, which needs no relay secret at all but
//!   the secure channel's roles reversed at the RPC layer, the pairing and
//!   the client's connection set-up reworked, and the home's member
//!   connections redone: larger than tickets, so later (the plan).
//! - Trade-offs: a member with no route depends on its home's relay and on
//!   reaching its home at least once a day; a member's ticket is a bearer
//!   credential for its own key only, of no use without that key's private
//!   half.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures_util::FutureExt as _;
use tokio_util::sync::CancellationToken;
use vessel_client::NodeClient;
use vessel_proto::{
    Callback, DeviceId, ErrorCode, GetCallback, NodeId, NodePublicKey, RelayToken, RouteUrl,
    WireError,
};
use vessel_relay::{LinkState, issue_ticket, unix_now};

use super::{Claim, Links, check_claim, hop_error, unavailable};
use crate::devices::Routes;
use crate::fleet::member::Membership;

/// How long a member device's ticket lasts once it is bound and holds a
/// grant: a member asks for a newer one halfway, so it stays registered
/// while it reaches its home at least that often.
const TICKET_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a ticket lasts before the device's first grant: enough to
/// register and grant; the member asks again halfway, bound by then.
const FIRST_TICKET_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// After a failed renewal, when a member asks again.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// The most of its home's relays a member dials: a home has a few.
pub(super) const MAX_CALLBACK_RELAYS: usize = 8;

/// How long a member waits for one of its home's relays to register it
/// before it grants (the home pairs through them).
const REGISTERED_WITHIN: Duration = Duration::from_secs(10);

/// Why a member's home cannot call it back, and how to fix it, for its log,
/// `vessel fleet status` and `vessel init` (the home's own check, "1 to 8
/// routes, not 0", said neither).
const NO_ROUTE: &str = "this node has no route its home could call it back on, so the home \
                        cannot run calls here (`--to`, `vessel mcp`'s `node`, `secret.copy`); \
                        syncing does not need one. The home has no relay to share: run the \
                        home with one (`vesseld --relay URL --relay-token-file FILE`), and its \
                        members register there too when they reconnect; or run this node with \
                        an address the home reaches (`vesseld --public ADDR`)";

/// What a home mints its members' tickets with: the relays it registers
/// with and their token, which never leaves it.
#[derive(Debug)]
pub(crate) struct HomeRelays {
    /// The relays, in its preference order.
    pub(crate) relays: Vec<RouteUrl>,
    /// Their registration token.
    pub(crate) token: RelayToken,
}

/// The home's side: devices given a ticket before their first grant, and
/// for which key (one each).
#[derive(Debug, Default)]
pub(super) struct FirstTickets(Mutex<HashMap<DeviceId, NodePublicKey>>);

/// The member's side: the ticket its callback links register with now.
#[derive(Debug, Default)]
pub(super) struct Ticketed(Mutex<Option<Issued>>);

#[derive(Debug, Clone, Copy)]
struct Issued {
    home: NodeId,
    /// When it was received and when it expires, in seconds since the epoch.
    received: u64,
    expires: u64,
}

impl Ticketed {
    fn set(&self, issued: Option<Issued>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = issued;
    }

    fn get(&self) -> Option<Issued> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn refused_ticket() -> WireError {
    WireError::new(
        ErrorCode::Forbidden,
        "this home gives a relay ticket only to a member device, for its own node and key"
            .to_owned(),
    )
}

impl Links {
    /// `fleet.callback` (the home), for member device `member`: its relays
    /// and a ticket for the member's key, if it may have one.
    pub(crate) async fn callback(
        &self,
        member: DeviceId,
        request: GetCallback,
    ) -> Result<Callback, WireError> {
        if !self.is_home() {
            return Err(unavailable("this node is not a fleet's home".to_owned()));
        }
        let Some(home_relays) = &self.home_relays else {
            return Ok(Callback {
                relays: Vec::new(),
                ticket: None,
            });
        };
        let lifetime = self.ticket_lifetime(member, &request).await?;
        let expires_at = unix_now().saturating_add(lifetime.as_secs());
        tracing::info!(device = %member, node = %request.node, lifetime = ?lifetime, "gave a member a relay ticket for its key");
        Ok(Callback {
            relays: home_relays.relays.clone(),
            ticket: Some(issue_ticket(&home_relays.token, &request.key, expires_at)),
        })
    }

    /// How long `member`'s ticket for `request` may last; refused when the
    /// device may have none (module Decision Log).
    async fn ticket_lifetime(
        &self,
        member: DeviceId,
        request: &GetCallback,
    ) -> Result<Duration, WireError> {
        let bound = self.bindings().await?;
        let held = self.grants().await;
        if let Some(node) = bound.node_of(member) {
            let current = held.iter().any(|grant| {
                grant.member == member && grant.node == node && grant.key == request.key
            });
            if node == request.node && bound.key_of(node) == Some(&request.key) && current {
                return Ok(TICKET_LIFETIME);
            }
            tracing::info!(device = %member, bound = %node, asked = %request.node, current, "refused a relay ticket: not the device's own node and key with a current grant");
            return Err(refused_ticket());
        }
        let claim = Claim {
            member,
            paired: self.paired_at(member).await,
            node: request.node,
            key: &request.key,
            name: None,
        };
        {
            let _granting = self.granting.lock().await;
            if let Err(error) = check_claim(claim, self.node, &self.name, &bound, &held) {
                tracing::info!(device = %member, node = %request.node, %error, "refused a first relay ticket");
                return Err(refused_ticket());
            }
        }
        let mut first = self
            .first_tickets
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match first.get(&member) {
            Some(key) if *key != request.key => {
                tracing::info!(device = %member, "refused a first relay ticket for a second key");
                Err(refused_ticket())
            }
            Some(_) | None => {
                let _previous = first.insert(member, request.key);
                Ok(FIRST_TICKET_LIFETIME)
            }
        }
    }

    /// What this member's grant names for its home to call it back: `None`
    /// for its own routes; otherwise its home's relays, linked with a
    /// ticket and one of them registered (or a while waited). The error
    /// says why the home cannot call it, and how to fix it.
    pub(super) async fn callback_routes(
        &self,
        client: &NodeClient,
        membership: &Membership,
        stop: &CancellationToken,
    ) -> Result<Option<Routes>, WireError> {
        let own = self.devices.routes();
        if !own.relays.is_empty() || !own.direct.is_empty() {
            return Ok(None);
        }
        let states = self.renew_callback(client, membership).await?;
        let registered = futures_util::future::select_ok(states.into_iter().map(|mut state| {
            async move {
                state
                    .wait_for(|state| *state == LinkState::Registered)
                    .await
                    .map(|_registered| ())
            }
            .boxed()
        }));
        let registered = tokio::select! {
            registered = tokio::time::timeout(REGISTERED_WITHIN, registered) => registered,
            () = stop.cancelled() => return Err(unavailable("this node is stopping".to_owned())),
        };
        match registered {
            Ok(Ok(((), _others))) => Ok(Some(Routes {
                relays: self.relays.callback_relays(membership.home),
                direct: Vec::new(),
            })),
            Ok(Err(_)) | Err(_) => Err(unavailable(
                "none of the home's relays has registered this node yet (its log says why); \
                 it keeps trying, and grants on its next connection to the home"
                    .to_owned(),
            )),
        }
    }

    /// Ask `membership`'s home for its relays and a ticket, and link to the
    /// ones this node dials with it; where each link stands.
    async fn renew_callback(
        &self,
        client: &NodeClient,
        membership: &Membership,
    ) -> Result<Vec<tokio::sync::watch::Receiver<LinkState>>, WireError> {
        let home = membership.home;
        let asked = GetCallback {
            node: self.node,
            key: *self.devices.key(),
        };
        let callback = client
            .call(asked)
            .await
            .map_err(|error| hop_error(home, error))?;
        let Some(ticket) = callback.ticket.filter(|_| !callback.relays.is_empty()) else {
            self.relays.keep_callback_of(None);
            self.ticketed.set(None);
            return Err(unavailable(NO_ROUTE.to_owned()));
        };
        let (dialed, left_out) = dialable(&callback.relays, membership);
        if !left_out.is_empty() {
            tracing::warn!(%home, relays = ?left_out, "left out relays of the home this node does not dial: only wss:// to a public address, or the host it reaches its home by");
        }
        if dialed.is_empty() {
            self.relays.keep_callback_of(None);
            self.ticketed.set(None);
            return Err(unavailable(format!(
                "the home's relays are not ones this node dials (only `wss://` to a public \
                 address, or the host it reaches its home by): {}; {NO_ROUTE}",
                left_out
                    .iter()
                    .map(RouteUrl::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let expires = ticket.expires_at;
        let states = self
            .relays
            .link_callback(home, &dialed, ticket)
            .map_err(|error| unavailable(format!("cannot link to the home's relays: {error}")))?;
        self.ticketed.set(Some(Issued {
            home,
            received: unix_now(),
            expires,
        }));
        Ok(states)
    }

    /// While connected to `membership`'s home through `client`, ask for a
    /// newer ticket halfway to the current one's expiry (a minute after a
    /// failure). Never returns: the connection's end stops it.
    pub(crate) async fn keep_callback(&self, client: &NodeClient, membership: &Membership) {
        let mut retry = None;
        loop {
            let due = match (retry.take(), self.ticketed.get()) {
                (Some(retry), _) => retry,
                (None, Some(issued)) if issued.home == membership.home => {
                    let half = issued
                        .expires
                        .saturating_sub(issued.received)
                        .checked_div(2)
                        .unwrap_or_default();
                    Duration::from_secs(
                        issued
                            .received
                            .saturating_add(half)
                            .saturating_sub(unix_now()),
                    )
                }
                // No callback links for this home: nothing to keep.
                (None, Some(_) | None) => std::future::pending::<Duration>().await,
            };
            tokio::time::sleep(due).await;
            if let Err(error) = self.renew_callback(client, membership).await {
                tracing::warn!(home = %membership.home, %error, "could not renew this node's relay ticket; trying again in a minute");
                retry = Some(RETRY_AFTER);
            }
        }
    }
}

/// Of the relays a home named, those this member dials (at most
/// [`MAX_CALLBACK_RELAYS`]), and those it leaves out: `wss://` to a public
/// host, or any on a host the member reaches its home by.
pub(super) fn dialable(
    offered: &[RouteUrl],
    membership: &Membership,
) -> (Vec<RouteUrl>, Vec<RouteUrl>) {
    let home_hosts: Vec<String> = membership
        .relays
        .iter()
        .chain(&membership.direct)
        .filter_map(host_of)
        .collect();
    let (mut dialed, left_out): (Vec<_>, Vec<_>) = offered.iter().cloned().partition(|relay| {
        host_of(relay)
            .is_some_and(|host| (relay.is_tls() && !is_local(&host)) || home_hosts.contains(&host))
    });
    dialed.truncate(MAX_CALLBACK_RELAYS);
    (dialed, left_out)
}

/// A route's host, lowercased, without brackets or port.
fn host_of(route: &RouteUrl) -> Option<String> {
    let rest = route.as_str().split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_userinfo, host)| host);
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split_once(']')?.0,
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _port)| host),
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `host` is this machine or a private network: loopback, private,
/// link-local, unique-local or unspecified addresses, and `localhost`.
fn is_local(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(IpAddr::V6(v6)) => {
            let first = v6.segments().first().copied().unwrap_or(0);
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_private() || v4.is_link_local())
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn membership(routes: &[&str]) -> Membership {
        Membership {
            home: NodeId::random(),
            key: NodePublicKey::from_bytes([1; 32]),
            relays: routes.iter().map(|route| route.parse().unwrap()).collect(),
            direct: Vec::new(),
            joined_at: Utc::now(),
            grant: Vec::new(),
        }
    }

    fn routes(routes: &[&str]) -> Vec<RouteUrl> {
        routes.iter().map(|route| route.parse().unwrap()).collect()
    }

    #[test]
    fn a_member_dials_only_public_wss_relays_or_its_homes_own_host() {
        let reached_by = membership(&["wss://home.example/vessel"]);
        let (dialed, left_out) = dialable(
            &routes(&[
                "wss://relay.example/vessel",
                "ws://relay.example:7078",
                "wss://127.0.0.1:7078",
                "ws://10.0.0.5:7078",
                "wss://[fd00::1]:443",
                "wss://localhost:7078",
                "wss://192.168.1.2",
            ]),
            &reached_by,
        );
        assert_eq!(dialed, routes(&["wss://relay.example/vessel"]));
        assert_eq!(left_out.len(), 6);

        // The host it reaches its home by: the same machine, or network.
        let local = membership(&["ws://127.0.0.1:7077"]);
        let (dialed, left_out) = dialable(
            &routes(&["ws://127.0.0.1:7078", "ws://127.0.0.2:7078"]),
            &local,
        );
        assert_eq!(dialed, routes(&["ws://127.0.0.1:7078"]));
        assert_eq!(left_out, routes(&["ws://127.0.0.2:7078"]));
    }

    #[test]
    fn a_member_dials_at_most_eight_of_its_homes_relays() {
        let offered: Vec<RouteUrl> = (0..20)
            .map(|n| format!("wss://relay{n}.example/vessel").parse().unwrap())
            .collect();
        let (dialed, _left_out) = dialable(&offered, &membership(&[]));
        assert_eq!(dialed.len(), MAX_CALLBACK_RELAYS);
    }

    #[test]
    fn hosts_are_read_without_port_brackets_or_credentials() {
        let host = |route: &str| host_of(&route.parse().unwrap());
        assert_eq!(host("ws://127.0.0.1:7078/x").as_deref(), Some("127.0.0.1"));
        assert_eq!(host("wss://[::1]:443").as_deref(), Some("::1"));
        assert_eq!(
            host("wss://user@Relay.Example").as_deref(),
            Some("relay.example")
        );
        assert!(is_local("::ffff:192.168.0.1"));
        assert!(!is_local("relay.example"));
        assert!(!is_local("203.0.113.9"));
    }
}
