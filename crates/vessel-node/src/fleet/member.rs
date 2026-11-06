//! The member role: joining a home ([`join`]), leaving ([`leave`]), and
//! the loop that pulls the home's bundle, applies it and reports
//! ([`run`]).
//!
//! [Decision Log]
//! - Membership is a pairing (ADR 0012): the home makes an invitation
//!   with the `sync` scope (`vessel fleet invite`), and `join` redeems it
//!   with `vessel_client::secure::pair_over` exactly as a device does. The
//!   credential and the home's routes are stored in the data directory
//!   (`fleet/member.credential`, `fleet/member.json`, owner-only), where
//!   the node finds them within a few seconds, running or not.
//! - The link is a single-use secret: `join` takes the decoded offer, and
//!   the CLI reads the link from standard input, never from the command
//!   line (`vessel fleet join`, as `vessel nodes add --pair`).
//! - When: on start (and whenever the connection comes back), when
//!   `fleet.watch` answers with another generation, and every 10 minutes
//!   regardless (`FleetConfig::pull_every`), which also notices local
//!   changes such as a restored file. Each pull fetches the manifest, the
//!   blobs the member lacks (chunk by chunk, hash-checked), applies into
//!   every profile's home, and reports the generation and the conflicted
//!   bundle paths.
//! - Failures: a connection that cannot be made or is lost is retried with
//!   the client's backoff (0.5 s to 30 s, jittered); a failed pull waits
//!   the same before the next, so a broken disk does not spin. Each is in
//!   `state.json` for `vessel fleet status`.
//! - Not in F2: the mutual grant (the member issuing the home a
//!   credential on itself, ADR 0012) needs a way to hand it over that
//!   protocol 5 does not have; it comes with cross-node calls (Track X).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use vessel_client::secure::{
    DialError, ReachError, RouteTimeouts, SecureConnector, TlsError, WsDialer, pair_over,
};
use vessel_client::{
    Backoff, CallError, Connector, CredentialError, CredentialMismatch, DeviceCredential,
    NodeClient, Pinned, SecureTarget,
};
use vessel_fleet::BlobStore;
use vessel_proto::{
    BlobChunk, ClientInfo, Generation, Manifest, NodeId, NodeInfo, NodePublicKey, PairingOffer,
    RouteUrl,
};

use super::files::{self, FileError, FleetFiles};
use super::local::Local;

/// The home this node is a member of (`fleet/member.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Membership {
    /// The home node.
    pub(crate) home: NodeId,
    /// Its key.
    pub(crate) key: NodePublicKey,
    /// Its relays.
    #[serde(default)]
    pub(crate) relays: Vec<RouteUrl>,
    /// Its direct addresses.
    #[serde(default)]
    pub(crate) direct: Vec<RouteUrl>,
    /// When this node joined.
    pub(crate) joined_at: DateTime<Utc>,
}

impl Membership {
    fn target(&self) -> SecureTarget {
        SecureTarget {
            key: self.key,
            relays: self.relays.clone(),
            direct: self.direct.clone(),
        }
    }
}

/// A node joined a fleet.
#[derive(Debug, Clone)]
pub struct Joined {
    /// The home, as it introduced itself.
    pub home: NodeInfo,
    /// The route the pairing took.
    pub route: String,
}

/// Why `join` did not join.
#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    /// This node is a fleet's home.
    #[error("this node is a fleet's home (it has fleet.toml); a home does not join another")]
    Home,
    /// This node is a member already.
    #[error("this node is already a member of a fleet; `vessel fleet leave` first")]
    Member,
    /// No TLS for a `wss://` route.
    #[error("TLS for the home's relays: {0}")]
    Tls(#[source] TlsError),
    /// The home could not be reached, or refused the link.
    #[error("{0}")]
    Reach(#[source] ReachError<DialError>),
    /// The credential or membership could not be written.
    #[error(transparent)]
    File(#[from] FileError),
    /// The credential could not be written.
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

/// Join the fleet whose home made `offer` (a link with the `sync` scope,
/// from `vessel fleet invite` on the home): pair with it as `device_name`
/// and store the credential and the home's routes in `files`. The node
/// starts pulling within a few seconds, or when it next starts.
///
/// The caller shows the offer (node, key fingerprint, routes) and asks
/// for consent before calling; the link must come from standard input or
/// a prompt, never the command line.
///
/// # Errors
/// See [`JoinError`].
pub async fn join(
    files: &FleetFiles,
    offer: &PairingOffer,
    device_name: &str,
) -> Result<Joined, JoinError> {
    if files.sources().exists() {
        return Err(JoinError::Home);
    }
    if files.membership().exists() {
        return Err(JoinError::Member);
    }
    let dialer =
        WsDialer::for_routes(offer.direct.iter().chain(&offer.relays)).map_err(JoinError::Tls)?;
    let (paired, route) = pair_over(&dialer, offer, device_name, RouteTimeouts::default())
        .await
        .map_err(JoinError::Reach)?;
    let membership = Membership {
        home: offer.node,
        key: offer.key,
        relays: offer.relays.clone(),
        direct: offer.direct.clone(),
        joined_at: Utc::now(),
    };
    let files = files.clone();
    let credential = paired.credential;
    tokio::task::spawn_blocking(move || -> Result<(), JoinError> {
        files::create_private_dir(files.dir())
            .map_err(|error| FileError::io(files.dir(), error))?;
        // The credential first: a membership without it would be unusable.
        credential.write(&files.credential())?;
        files::write_json(&files.membership(), &membership)?;
        Ok(())
    })
    .await
    .map_err(|error| {
        FileError::io(std::path::Path::new("fleet"), std::io::Error::other(error))
    })??;
    Ok(Joined {
        home: paired.node,
        route: route.to_string(),
    })
}

/// Stop being a member: forget the home and this node's credential on it.
/// The files Vessel wrote stay, as they are. Whether this node was one.
/// Blocks.
///
/// # Errors
/// A file could not be removed.
pub fn leave(files: &FleetFiles) -> Result<bool, FileError> {
    let was = files.membership().exists();
    files::remove(&files.membership())?;
    files::remove(&files.credential())?;
    Ok(was)
}

/// Why the node cannot pull with its membership.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MembershipError {
    #[error(transparent)]
    File(#[from] FileError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error("the credential is not for the home in member.json: {0}")]
    Mismatch(#[from] CredentialMismatch),
    #[error("TLS for the home's relays: {0}")]
    Tls(#[source] TlsError),
}

/// The membership in `files`, if any. Blocks.
pub(crate) fn membership(files: &FleetFiles) -> Result<Option<Membership>, FileError> {
    files::read_json(&files.membership())
}

/// The connector to the home of `membership`. Blocks (reads the
/// credential).
pub(crate) fn connector(
    files: &FleetFiles,
    membership: &Membership,
) -> Result<Pinned<SecureConnector<WsDialer>>, MembershipError> {
    let credential = DeviceCredential::read(&files.credential())?;
    let target = membership.target();
    let dialer = WsDialer::for_routes(target.routes()).map_err(MembershipError::Tls)?;
    let connector = SecureConnector::new(dialer, target, membership.home, credential)?;
    Ok(Pinned::new(connector, Some(membership.home)))
}

/// How the pull loop runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pacing {
    /// Pull at least this often.
    pub(crate) pull_every: Duration,
    /// Waits after failures.
    pub(crate) backoff: Backoff,
}

fn client_info() -> ClientInfo {
    ClientInfo {
        name: "vesseld-fleet".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Why a pull failed.
#[derive(Debug, thiserror::Error)]
enum PullError {
    #[error("the home: {0}")]
    Call(#[from] CallError),
    #[error("a blob: {0}")]
    Blob(#[source] std::io::Error),
    #[error("a blob arrived longer than the home said ({0} bytes)")]
    Oversized(u64),
    #[error(transparent)]
    File(#[from] FileError),
    #[error(transparent)]
    Apply(#[from] super::local::LocalError),
    #[error("a blocking task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Pull from the home through `connector` until `stop`.
pub(crate) async fn run<C: Connector>(
    connector: C,
    local: Arc<Local>,
    pacing: Pacing,
    stop: CancellationToken,
) {
    let mut failures = 0_u32;
    let mut applied = local.state.get().generation;
    while !stop.is_cancelled() {
        let client = tokio::select! {
            connected = connector.connect(client_info()) => connected,
            () = stop.cancelled() => return,
        };
        let client = match client {
            Ok(client) => client,
            Err(error) => {
                let message = format!("cannot reach the home: {error}");
                tracing::warn!("{message}");
                note_error(&local, message).await;
                failures = failures.saturating_add(1);
                pause(pacing.backoff.delay(failures), &stop).await;
                continue;
            }
        };
        failures = 0;
        tracing::info!(home = %client.node().id, "connected to the fleet's home");
        session(&client, &local, pacing, &mut applied, &stop).await;
    }
}

/// One connection's worth: pull, then wait for news or the timer.
async fn session(
    client: &NodeClient,
    local: &Arc<Local>,
    pacing: Pacing,
    applied: &mut Option<Generation>,
    stop: &CancellationToken,
) {
    let mut failures = 0_u32;
    let mut due = true;
    let mut timer = tokio::time::interval(pacing.pull_every);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let _first = timer.tick().await;
    loop {
        if due {
            match pull(client, local).await {
                Ok(generation) => {
                    *applied = Some(generation);
                    failures = 0;
                }
                Err(PullError::Call(error)) if is_lost(&error) => {
                    tracing::info!(%error, "lost the fleet's home; reconnecting");
                    return;
                }
                Err(error) => {
                    let message = format!("pulling from the home: {error}");
                    tracing::warn!("{message}");
                    note_error(local, message).await;
                    failures = failures.saturating_add(1);
                    pause(pacing.backoff.delay(failures), stop).await;
                    if stop.is_cancelled() {
                        return;
                    }
                    continue;
                }
            }
        }
        let after = applied.unwrap_or_default();
        tokio::select! {
            answer = client.fleet_watch(after) => match answer {
                Ok(generation) => {
                    contacted(local).await;
                    due = Some(generation) != *applied;
                }
                Err(error) if is_lost(&error) => {
                    tracing::info!(%error, "lost the fleet's home; reconnecting");
                    return;
                }
                Err(error) => {
                    let message = format!("watching the home: {error}");
                    tracing::warn!("{message}");
                    note_error(local, message).await;
                    pause(pacing.backoff.delay(1), stop).await;
                    due = false;
                }
            },
            _tick = timer.tick() => due = true,
            () = stop.cancelled() => return,
            () = client.closed() => return,
        }
    }
}

/// Whether the connection is gone (reconnect), rather than one call
/// failing.
const fn is_lost(error: &CallError) -> bool {
    matches!(error, CallError::Disconnected)
}

/// Fetch the manifest and the blobs this node lacks, apply, report.
async fn pull(client: &NodeClient, local: &Arc<Local>) -> Result<Generation, PullError> {
    let manifest = client.fleet_manifest().await?;
    contacted(local).await;
    let blobs = {
        let path = local.files.blobs();
        tokio::task::spawn_blocking(move || BlobStore::open(path))
            .await?
            .map_err(PullError::Blob)?
    };
    fetch(client, &manifest, &blobs).await?;
    let outcome = {
        let local = Arc::clone(local);
        let manifest = manifest.clone();
        let blobs = blobs.clone();
        tokio::task::spawn_blocking(move || -> Result<_, PullError> {
            files::write_json(&local.files.manifest(), &manifest)?;
            let outcome = local.apply(&manifest, &blobs, &[])?;
            let keep = manifest.files.values().map(|file| file.hash).collect();
            if let Err(error) = blobs.retain(&keep) {
                tracing::warn!(%error, "could not clean the fleet's blob store");
            }
            Ok(outcome)
        })
        .await??
    };
    client
        .fleet_report(manifest.generation, outcome.conflicts)
        .await?;
    Ok(manifest.generation)
}

/// Fetch every blob of `manifest` that `blobs` lacks, chunk by chunk.
async fn fetch(
    client: &NodeClient,
    manifest: &Manifest,
    blobs: &BlobStore,
) -> Result<(), PullError> {
    let mut wanted: Vec<_> = manifest.files.values().collect();
    wanted.sort_by_key(|file| file.hash);
    wanted.dedup_by_key(|file| file.hash);
    for file in wanted {
        if blobs.contains(&file.hash) {
            continue;
        }
        let mut writer = Some(blobs.writer(file.hash).map_err(PullError::Blob)?);
        let mut offset = 0_u64;
        loop {
            let BlobChunk { data, total } = client.fleet_blob(file.hash, offset).await?;
            let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
            offset = offset.saturating_add(length);
            if offset > file.size || offset > total {
                return Err(PullError::Oversized(offset));
            }
            let mut chunk_writer = writer.take().ok_or(PullError::Oversized(offset))?;
            chunk_writer = tokio::task::spawn_blocking(move || {
                std::io::Write::write_all(&mut chunk_writer, &data).map(|()| chunk_writer)
            })
            .await?
            .map_err(PullError::Blob)?;
            if data_done(offset, total, length) {
                let _stored = tokio::task::spawn_blocking(move || chunk_writer.finish())
                    .await?
                    .map_err(PullError::Blob)?;
                break;
            }
            writer = Some(chunk_writer);
        }
    }
    Ok(())
}

/// Whether a blob is complete: all of it arrived, or the home sent an
/// empty chunk (nothing more will come).
const fn data_done(offset: u64, total: u64, length: u64) -> bool {
    offset >= total || length == 0
}

async fn note_error(local: &Arc<Local>, message: String) {
    let local = Arc::clone(local);
    let _written = tokio::task::spawn_blocking(move || {
        local.state.update(|state| state.error = Some(message));
    })
    .await;
}

async fn contacted(local: &Arc<Local>) {
    let local = Arc::clone(local);
    let _written = tokio::task::spawn_blocking(move || {
        local
            .state
            .update(|state| state.contacted_at = Some(Utc::now()));
    })
    .await;
}

/// Wait `delay`, or until `stop`.
async fn pause(delay: Duration, stop: &CancellationToken) {
    tokio::select! {
        () = tokio::time::sleep(delay) => {}
        () = stop.cancelled() => {}
    }
}
