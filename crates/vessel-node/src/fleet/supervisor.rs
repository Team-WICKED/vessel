//! The node's fleet loop: every `poll` it reads the fleet files and acts
//! on what changed (see the module's Decision Log in `fleet`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use vessel_client::Backoff;
use vessel_fleet::{BlobStore, Sources};
use vessel_proto::{Manifest, McpDefinitions};

use super::FleetConfig;
use super::config::read_sources;
use super::files::{self, FleetFiles};
use super::home::{Home, Published, Stamp};
use super::links::Links;
use super::local::Local;
use super::member::{self, Membership, Pacing};
use super::secrets::{self, Secrets, SecretsError};

/// The home publishes at least this often, whatever the stamp says.
const REBUNDLE_EVERY: Duration = Duration::from_secs(10 * 60);

/// What the loop keeps between polls.
struct Supervisor {
    local: Arc<Local>,
    home: Arc<Home>,
    links: Arc<Links>,
    config: FleetConfig,
    user_home: Option<PathBuf>,
    tasks: TaskTracker,
    stop: CancellationToken,
    /// The home's last stamp and when it last bundled.
    stamp: Option<(Stamp, Instant)>,
    /// The member loop running, for which membership.
    member: Option<(Membership, CancellationToken)>,
    /// The last error logged, so a lasting one is logged once.
    logged: Option<String>,
    /// The SHA-256 of `secrets.toml` last used, to tell a change.
    secrets: Option<[u8; 32]>,
    /// Whether this node was in a fleet at the last look (its sessions get
    /// the fleet's MCP servers only while it is).
    in_fleet: bool,
    /// Whether the saved manifest's MCP servers were taken up after start.
    started: bool,
}

/// Run the fleet loop until `stop`.
pub(crate) async fn run(
    local: Arc<Local>,
    home: Arc<Home>,
    links: Arc<Links>,
    config: FleetConfig,
    user_home: Option<PathBuf>,
    tasks: TaskTracker,
    stop: CancellationToken,
) {
    let mut supervisor = Supervisor {
        local,
        home,
        links,
        config,
        user_home,
        tasks,
        stop: stop.clone(),
        stamp: None,
        member: None,
        logged: None,
        secrets: None,
        in_fleet: false,
        started: false,
    };
    loop {
        supervisor.tick().await;
        tokio::select! {
            () = tokio::time::sleep(supervisor.config.poll) => {}
            () = stop.cancelled() => break,
        }
    }
    if let Some((_membership, member_stop)) = supervisor.member.take() {
        member_stop.cancel();
    }
}

impl Supervisor {
    async fn tick(&mut self) {
        let files = self.local.files.clone();
        let user_home = self.user_home.clone();
        let started = self.started;
        let read = tokio::task::spawn_blocking(move || {
            let sources = read_sources(&files, user_home.as_deref());
            let membership = member::membership(&files);
            let requested = files.apply_request().exists();
            let secrets = secrets::read(&files);
            // After a start, sessions get the servers of the manifest last
            // applied here before the home answers (or the home bundles).
            let saved = if started {
                None
            } else {
                files::read_json::<Manifest>(&files.manifest())
                    .ok()
                    .flatten()
            };
            (sources, membership, requested, secrets, saved)
        })
        .await;
        let Ok((sources, membership, requested, secrets, saved)) = read else {
            return;
        };
        self.use_secrets(secrets).await;
        // A grant to a home this node no longer follows is revoked.
        self.links.reconcile().await;
        let in_fleet = matches!(sources, Ok(Some(_))) || matches!(membership, Ok(Some(_)));
        if !self.started {
            self.started = true;
            if let (true, Some(saved)) = (in_fleet, saved) {
                self.local.mcp.set_definitions(saved.mcp);
            }
        }
        if self.in_fleet && !in_fleet {
            // Left the fleet: its MCP servers go with it.
            self.local.mcp.set_definitions(McpDefinitions::new());
        }
        self.in_fleet = in_fleet;
        match sources {
            Ok(Some((text, sources))) => {
                self.stop_member();
                self.publish(&text, &sources, requested).await;
                return;
            }
            Ok(None) => {
                if self.home.current().is_some() {
                    tracing::info!("fleet.toml is gone: this node is no longer the fleet's home");
                    self.home.publish(None);
                    self.stamp = None;
                }
            }
            Err(error) => {
                // A broken edit keeps the previous bundle published.
                self.note(format!("fleet.toml: {error}")).await;
                return;
            }
        }
        match membership {
            Ok(Some(membership)) => {
                self.follow(membership).await;
                if requested {
                    self.apply_stored().await;
                }
            }
            Ok(None) => self.stop_member(),
            Err(error) => self.note(format!("member.json: {error}")).await,
        }
    }

    /// Take up `secrets.toml` if its text changed.
    async fn use_secrets(&mut self, read: Result<(Secrets, [u8; 32]), SecretsError>) {
        match read {
            Ok((secrets, digest)) => {
                if self.secrets != Some(digest) {
                    let names: Vec<String> = secrets.names().map(ToString::to_string).collect();
                    tracing::info!(secrets = ?names, "this node's secrets");
                    self.local.mcp.set_secrets(secrets);
                    self.secrets = Some(digest);
                }
            }
            // The secrets in use stay; the error names no value.
            Err(error) => self.note(format!("secrets.toml: {error}")).await,
        }
    }

    /// The home: bundle if the sources changed (or on the timer, or when
    /// asked), publish, apply here too.
    async fn publish(&mut self, text: &str, sources: &Sources, requested: bool) {
        let stamp = {
            let text = text.to_owned();
            let sources = sources.clone();
            match tokio::task::spawn_blocking(move || Stamp::of(&text, &sources)).await {
                Ok(stamp) => stamp,
                Err(_panicked) => return,
            }
        };
        let fresh = self.stamp.as_ref().is_some_and(|(last, at)| {
            *last == stamp && at.elapsed() < REBUNDLE_EVERY && self.home.current().is_some()
        });
        if fresh && !requested {
            return;
        }
        let files = self.local.files.clone();
        let local = Arc::clone(&self.local);
        let sources = sources.clone();
        let previous = self.home.current();
        let bundled = tokio::task::spawn_blocking(move || {
            publish_blocking(&files, &local, &sources, previous.as_deref(), requested)
        })
        .await;
        match bundled {
            Ok(Ok(published)) => {
                self.stamp = Some((stamp, Instant::now()));
                self.logged = None;
                self.home.publish(Some(published));
            }
            Ok(Err(message)) => self.note(message).await,
            Err(error) => self.note(format!("bundling failed: {error}")).await,
        }
    }

    /// The member: run the pull loop for `membership`, restarting it if
    /// the membership changed.
    async fn follow(&mut self, membership: Membership) {
        if self
            .member
            .as_ref()
            .is_some_and(|(current, _stop)| *current == membership)
        {
            return;
        }
        self.stop_member();
        let made = {
            let files = self.local.files.clone();
            let membership = membership.clone();
            tokio::task::spawn_blocking(move || member::connector(&files, &membership)).await
        };
        let Ok(made) = made else {
            return;
        };
        let connector = match made {
            Ok(connector) => connector,
            Err(error) => {
                self.note(format!("cannot use the fleet membership: {error}"))
                    .await;
                return;
            }
        };
        tracing::info!(home = %membership.home, "following the fleet's home");
        let member_stop = self.stop.child_token();
        let pacing = Pacing {
            pull_every: self.config.pull_every,
            backoff: Backoff::default(),
        };
        drop(self.tasks.spawn(member::run(
            connector,
            membership.clone(),
            Arc::clone(&self.local),
            Arc::clone(&self.links),
            pacing,
            member_stop.clone(),
        )));
        self.member = Some((membership, member_stop));
    }

    fn stop_member(&mut self) {
        if let Some((membership, member_stop)) = self.member.take() {
            tracing::info!(home = %membership.home, "no longer following the fleet's home");
            member_stop.cancel();
        }
    }

    /// A member asked to apply again (`vessel fleet restore`): apply the
    /// manifest it has, without the home.
    async fn apply_stored(&mut self) {
        let local = Arc::clone(&self.local);
        let applied = tokio::task::spawn_blocking(move || -> Result<(), String> {
            files::remove(&local.files.apply_request()).map_err(|error| error.to_string())?;
            let Some(manifest) = files::read_json::<Manifest>(&local.files.manifest())
                .map_err(|error| error.to_string())?
            else {
                return Ok(());
            };
            let blobs = BlobStore::open(local.files.blobs()).map_err(|error| error.to_string())?;
            local
                .apply(&manifest, &blobs, &[])
                .map(drop)
                .map_err(|error| error.to_string())
        })
        .await;
        match applied {
            Ok(Ok(())) => {}
            Ok(Err(message)) => self.note(message).await,
            Err(error) => self.note(format!("applying failed: {error}")).await,
        }
    }

    /// Record `message` as the fleet's error; log it once while it lasts.
    async fn note(&mut self, message: String) {
        if self.logged.as_ref() == Some(&message) {
            return;
        }
        tracing::warn!("{message}");
        self.logged = Some(message.clone());
        let local = Arc::clone(&self.local);
        let _written = tokio::task::spawn_blocking(move || {
            local.state.update(|state| state.error = Some(message));
        })
        .await;
    }
}

/// Bundle `sources` into the home's store, save the manifest, clean the
/// store and apply here. Blocks. The message is what went wrong.
fn publish_blocking(
    files: &FleetFiles,
    local: &Local,
    sources: &Sources,
    previous: Option<&Published>,
    requested: bool,
) -> Result<Arc<Published>, String> {
    let blobs = BlobStore::open(files.blobs()).map_err(|error| format!("blob store: {error}"))?;
    let saved: Option<Manifest> = match previous {
        Some(previous) => Some(previous.manifest.clone()),
        // After a restart: the generation goes on from the saved manifest.
        None => files::read_json(&files.manifest()).map_err(|error| error.to_string())?,
    };
    let bundled = vessel_fleet::bundle(sources, &blobs, saved.as_ref())
        .map_err(|error| format!("bundling: {error}"))?;
    let problems: Vec<String> = bundled.problems.iter().map(ToString::to_string).collect();
    for problem in &problems {
        tracing::warn!(%problem, "left out of the fleet bundle");
    }
    let changed = bundled.changed || previous.is_none();
    if bundled.changed {
        files::write_json(&files.manifest(), &bundled.manifest)
            .map_err(|error| error.to_string())?;
        tracing::info!(
            generation = bundled.manifest.generation.get(),
            files = bundled.manifest.files.len(),
            "published a fleet bundle"
        );
    }
    let keep = bundled
        .manifest
        .files
        .values()
        .map(|file| file.hash)
        .collect();
    if let Err(error) = blobs.retain(&keep) {
        tracing::warn!(%error, "could not clean the fleet's blob store");
    }
    if requested {
        files::remove(&files.apply_request()).map_err(|error| error.to_string())?;
    }
    if changed || requested {
        // The home's own profiles get the bundle too; its sources (and
        // homes linked to them) are left alone by the plan.
        let _outcome = local
            .apply(&bundled.manifest, &blobs, &sources.paths())
            .map_err(|error| format!("applying here: {error}"))?;
    }
    local.state.update(|state| state.problems = problems);
    Ok(Arc::new(Published::new(bundled.manifest, blobs)))
}
