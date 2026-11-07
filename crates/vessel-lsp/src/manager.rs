//! Bounded root/language actors. A server owns all mutable LSP state.
//!
//! Servers are keyed by the workspace they analyse and accounted to the
//! allowed root the node granted, so slots are shared fairly between roots
//! (see `Pool`). Each server admits one active query and eight
//! queued queries; overload is refused. The manager never waits for a query
//! while routing another server. Idle and backoff timers are deadlines, not
//! filesystem/readiness polling.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use vessel_proto::{Reply, lsp::Language};

use crate::error::LspError as Error;
use crate::process::{Launch, Process};
use crate::protocol::initialize as init;
use crate::query::Query;
use crate::registry::Registry;
use crate::sync::Watcher;
use crate::toolchain::RustTools;

/// Resource limits and trusted server selection, configured only by the node.
#[derive(Debug, Clone)]
pub struct Config {
    /// Trusted executables, empty by default so library tests never find real tools.
    pub registry: Registry,
    /// Concurrent servers, including those starting (default four).
    pub max_servers: NonZeroUsize,
    /// Time without queries before shutdown (default five minutes).
    pub idle_timeout: Duration,
    /// Whole-query and initialization deadline (default thirty seconds).
    pub request_timeout: Duration,
    /// First crash/startup retry delay, doubling to at most thirty seconds.
    pub restart_backoff: Duration,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            registry: Registry::default(),
            max_servers: NonZeroUsize::MIN.saturating_add(3),
            idle_timeout: Duration::from_secs(300),
            request_timeout: Duration::from_secs(30),
            restart_backoff: Duration::from_secs(1),
        }
    }
}

/// Where one query runs, as authorized by the node.
///
/// A server analyses `workspace`, the canonical directory the caller asked
/// for. `root` is the allowed root the node granted for it (a configured
/// root or a local session's cwd): server slots are shared fairly between
/// allowed roots, so many subdirectories of one root cannot lock another
/// root out.
#[derive(Debug, Clone)]
pub struct Scope {
    root: PathBuf,
    workspace: PathBuf,
    full_analysis: bool,
}

impl Scope {
    /// Both paths canonical; `workspace` at or below `root`.
    /// `full_analysis` is the operator's choice for `root`, never a caller's.
    ///
    /// # Errors
    /// [`Error::Path`] when `workspace` is outside `root`.
    pub fn new(root: PathBuf, workspace: PathBuf, full_analysis: bool) -> Result<Self, Error> {
        if !workspace.starts_with(&root) {
            return Err(Error::Path);
        }
        Ok(Self {
            root,
            workspace,
            full_analysis,
        })
    }
}

/// A server's identity: what it analyses, never who asked.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    workspace: PathBuf,
    language: Language,
    full: bool,
}
#[derive(Debug)]
struct Work {
    query: Query,
    reply: oneshot::Sender<Result<Reply, Error>>,
}
#[derive(Debug)]
struct Admission {
    key: Key,
    /// The allowed root this admission's slot is accounted to.
    root: PathBuf,
    work: Work,
}

/// The node's Rust toolchain, resolved at the first Rust launch and then
/// shared by every server of this manager (see `toolchain`).
type Tools = Arc<tokio::sync::OnceCell<RustTools>>;

/// Cheap sender to the node's LSP manager. Authority is checked by the node.
#[derive(Debug, Clone)]
pub struct Manager {
    sender: mpsc::Sender<Admission>,
    finished: tokio::sync::watch::Receiver<bool>,
}

impl Manager {
    /// Start a manager under the node's cancellation tree.
    #[must_use]
    pub fn start(config: Config, shutdown: CancellationToken) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        let (done, finished) = tokio::sync::watch::channel(false);
        drop(tokio::spawn(async move {
            Pool::new(config, shutdown).run(receiver).await;
            let _old = done.send_replace(true);
        }));
        Self { sender, finished }
    }

    /// Wait until node cancellation has shut down every language-server group.
    pub async fn stopped(&self) {
        let mut finished = self.finished.clone();
        while !*finished.borrow_and_update() {
            if finished.changed().await.is_err() {
                break;
            }
        }
    }

    /// Query within an already-authorized scope.
    ///
    /// # Errors
    /// Refuses overload, shutdown, unavailable servers and invalid source data.
    pub async fn query(&self, scope: Scope, query: Query) -> Result<Reply, Error> {
        if let Some(file) = query.file() {
            let _canonical = crate::documents::resolve(&scope.workspace, file).await?;
        }
        let (reply, response) = oneshot::channel();
        let key = Key {
            workspace: scope.workspace,
            language: query.workspace().language,
            full: scope.full_analysis,
        };
        self.sender
            .try_send(Admission {
                key,
                root: scope.root,
                work: Work { query, reply },
            })
            .map_err(|_full_or_closed| Error::Unavailable)?;
        response.await.map_err(|_closed| Error::Unavailable)?
    }
}

/// Admissions waiting for a slot a retiring server is about to free.
const MAX_PARKED: usize = 8;

#[derive(Debug)]
struct Slot {
    sender: mpsc::Sender<Work>,
    /// The allowed root this server is accounted to (its first requester's).
    root: PathBuf,
    /// Asks the server to stop between queries, keeping any in-flight one.
    retire: CancellationToken,
    retiring: bool,
    /// Admission order of the last query, for least-recently-used choice.
    used: u64,
}

/// [Decision Log]
/// - Purpose: a bounded number of servers, shared fairly between roots.
/// - Before: four subdirectories of one allowed root took every slot and
///   every other root was refused until they idled out (review finding).
/// - Alternatives: one server per allowed root, rooted there (a session cwd
///   such as a home directory would make pyright/gopls index the whole tree
///   and put user-installed tools "inside the root"); a fixed per-root quota
///   (wastes slots while one root is alone); refusing and asking callers to
///   retry (callers would have to poll for a slot).
/// - Chosen: servers keep the requested workspace as their analysis root
///   and are accounted to the allowed root. When all slots are taken, the
///   busiest root that holds more servers than the requesting root loses its
///   least recently used server; otherwise the requesting root recycles its
///   own. The admission waits (parked, bounded) for that slot.
/// - Trade-off: retirement waits for an in-flight query; queries already
///   queued on the retired server are answered `Unavailable`.
struct Pool {
    config: Config,
    shutdown: CancellationToken,
    servers: BTreeMap<Key, Slot>,
    parked: VecDeque<Admission>,
    tasks: JoinSet<()>,
    tools: Tools,
    clock: u64,
}

impl Pool {
    fn new(config: Config, shutdown: CancellationToken) -> Self {
        Self {
            config,
            shutdown,
            servers: BTreeMap::new(),
            parked: VecDeque::new(),
            tasks: JoinSet::new(),
            tools: Tools::default(),
            clock: 0,
        }
    }

    async fn run(mut self, mut inbox: mpsc::Receiver<Admission>) {
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => break,
                _ended = self.tasks.join_next(), if !self.tasks.is_empty() => self.unpark(),
                admission = inbox.recv() => {
                    let Some(admission) = admission else { break; };
                    self.admit(admission);
                }
            }
        }
        self.shutdown.cancel();
        drop(self.servers);
        drop(self.parked);
        while self.tasks.join_next().await.is_some() {}
    }

    /// A server ended: retry parked admissions once, in arrival order.
    fn unpark(&mut self) {
        for admission in std::mem::take(&mut self.parked) {
            if !admission.work.reply.is_closed() {
                self.admit(admission);
            }
        }
    }

    fn admit(&mut self, admission: Admission) {
        self.servers.retain(|_, slot| !slot.sender.is_closed());
        self.clock = self.clock.saturating_add(1);
        let Admission { key, root, work } = admission;
        if let Some(slot) = self.servers.get_mut(&key) {
            if slot.retiring {
                self.park(Admission { key, root, work });
            } else {
                slot.used = self.clock;
                if let Err(error) = slot.sender.try_send(work) {
                    drop(error.into_inner().reply.send(Err(Error::Unavailable)));
                }
            }
            return;
        }
        if self.servers.len() < self.config.max_servers.get() {
            self.spawn(key, root, work);
            return;
        }
        if let Some(victim) = self.victim(&root) {
            if let Some(slot) = self.servers.get_mut(&victim) {
                slot.retiring = true;
                slot.retire.cancel();
            }
            self.park(Admission { key, root, work });
        } else if self.servers.values().any(|slot| slot.retiring) {
            self.park(Admission { key, root, work });
        } else {
            drop(work.reply.send(Err(Error::Unavailable)));
        }
    }

    fn park(&mut self, admission: Admission) {
        if self.parked.len() >= MAX_PARKED {
            drop(admission.work.reply.send(Err(Error::Unavailable)));
        } else {
            self.parked.push_back(admission);
        }
    }

    /// The server to retire so `root` gets a slot: the least recently used
    /// one of the busiest root holding more servers than `root`, else the
    /// least recently used of `root`'s own. Retiring servers do not count.
    fn victim(&self, root: &Path) -> Option<Key> {
        let live = || self.servers.iter().filter(|(_, slot)| !slot.retiring);
        let mut held: BTreeMap<&Path, usize> = BTreeMap::new();
        for (_, slot) in live() {
            let count = held.entry(slot.root.as_path()).or_default();
            *count = count.saturating_add(1);
        }
        let mine = held.get(root).copied().unwrap_or(0);
        let most = held.values().copied().max().unwrap_or(0);
        let from = |chosen: &dyn Fn(&Slot) -> bool| {
            live()
                .filter(|(_, slot)| chosen(slot))
                .min_by_key(|(_, slot)| slot.used)
                .map(|(key, _)| key.clone())
        };
        if most > mine {
            from(&|slot| held.get(slot.root.as_path()) == Some(&most))
        } else {
            from(&|slot| slot.root == root)
        }
    }

    fn spawn(&mut self, key: Key, root: PathBuf, work: Work) {
        let (sender, receiver) = mpsc::channel(8);
        let retire = CancellationToken::new();
        let task = server(
            key.clone(),
            self.config.clone(),
            self.shutdown.child_token(),
            retire.clone(),
            receiver,
            Arc::clone(&self.tools),
        );
        drop(self.tasks.spawn(task));
        if let Err(error) = sender.try_send(work) {
            drop(error.into_inner().reply.send(Err(Error::Unavailable)));
        }
        drop(self.servers.insert(
            key,
            Slot {
                sender,
                root,
                retire,
                retiring: false,
                used: self.clock,
            },
        ));
    }
}

/// The project set and configuration identity a server generation uses.
#[derive(Debug, Default, PartialEq, Eq)]
struct Generation {
    projects: Vec<PathBuf>,
    stamp: Option<crate::policy::Stamp>,
}

/// Recheck the policy and identify the generation the server must run.
fn plan(key: &Key, home: Option<&Path>) -> Result<Generation, Error> {
    let projects = crate::policy::check(&key.workspace, key.language, key.full, home)?;
    let stamp = crate::policy::stamp(&key.workspace, &projects)?;
    Ok(Generation {
        projects,
        stamp: Some(stamp),
    })
}

async fn server(
    key: Key,
    config: Config,
    shutdown: CancellationToken,
    retire: CancellationToken,
    mut inbox: mpsc::Receiver<Work>,
    tools: Tools,
) {
    let Ok((watcher, mut changes)) = Watcher::new().await else {
        return;
    };
    let mut process: Option<Process> = None;
    let mut generation = Generation::default();
    let mut retry_at = Instant::now();
    let mut backoff = config.restart_backoff.min(Duration::from_secs(30));
    let mut idle_at = Instant::now()
        .checked_add(config.idle_timeout)
        .unwrap_or_else(Instant::now);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = retire.cancelled() => break,
            () = tokio::time::sleep_until(idle_at) => break,
            event = changes.changed(), if process.is_some() => {
                if event.is_err() { break; }
                drop(changes.borrow_and_update());
                if let Some(server) = &mut process {
                    let result = tokio::time::timeout(config.request_timeout, server.refresh_all(&watcher)).await;
                    if !matches!(result, Ok(Ok(()))) { process = None; }
                }
            }
            message = receive_event(&mut process) => {
                if let Some(server) = &mut process {
                    let result = match message { Some(Ok(message)) => tokio::time::timeout(config.request_timeout, server.event(message)).await.ok(), Some(Err(_)) | None => None };
                    if !matches!(result, Some(Ok(()))) {
                        process = None;
                        retry_at = Instant::now().checked_add(backoff).unwrap_or_else(Instant::now);
                        backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
                    }
                }
            }
            work = inbox.recv() => {
                let Some(mut work) = work else { break; };
                if work.reply.is_closed() { continue; }
                if Instant::now() < retry_at { drop(work.reply.send(Err(Error::Unavailable))); continue; }
                let policy_key = key.clone();
                let policy_home = config.registry.home.clone();
                let planned = tokio::task::spawn_blocking(move || {
                    plan(&policy_key, policy_home.as_deref())
                }).await;
                let next = match planned {
                    Ok(Ok(next)) => next,
                    Ok(Err(error)) => { process = None; drop(work.reply.send(Err(error))); continue; }
                    Err(error) => { process = None; drop(work.reply.send(Err(error.into()))); continue; }
                };
                if next != generation { process = None; }
                generation = next;
                if process.is_none() {
                    let started = tokio::select! {
                        () = shutdown.cancelled() => break,
                        _cancelled = work.reply.closed() => Err(Error::Unavailable),
                        result = launch(&key, &config, &tools, &generation.projects, &watcher) => result,
                    };
                    match started {
                        Ok(started) => process = Some(started),
                        Err(error) => {
                            retry_at = Instant::now().checked_add(backoff).unwrap_or_else(Instant::now);
                            backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
                            drop(work.reply.send(Err(error))); continue;
                        }
                    }
                }
                let Some(server) = &mut process else { break; };
                let mut reply = work.reply;
                let result = tokio::select! {
                    () = shutdown.cancelled() => break,
                    _cancelled = reply.closed() => Err(Error::Unavailable),
                    result = tokio::time::timeout(config.request_timeout, server.query(work.query, &watcher)) => result.unwrap_or(Err(Error::Timeout)),
                };
                let failed = matches!(&result, Err(Error::Protocol | Error::Timeout | Error::Frame(_) | Error::Json(_) | Error::Unavailable));
                if failed {
                    process = None;
                    retry_at = Instant::now().checked_add(backoff).unwrap_or_else(Instant::now);
                    backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
                } else { backoff = config.restart_backoff.min(Duration::from_secs(30)); }
                drop(reply.send(result));
                idle_at = Instant::now().checked_add(config.idle_timeout).unwrap_or_else(Instant::now);
            }
        }
    }
    if let Some(mut process) = process {
        process.shutdown().await;
    }
}

/// Start a new server generation: resolve the toolchain once, prepare the
/// launch off the async workers and initialize the process.
async fn launch(
    key: &Key,
    config: &Config,
    tools: &Tools,
    projects: &[PathBuf],
    watcher: &Watcher,
) -> Result<Process, Error> {
    // The previous generation's documents are gone with its process.
    watcher.reset().await?;
    // Conservative Rust only: full analysis keeps rustup's own toolchain
    // selection, project files included.
    let rust = if key.language == Language::Rust && !key.full {
        let registry = config.registry.clone();
        let deadline = config.request_timeout;
        let resolved = tools
            .get_or_try_init(|| async move {
                let state = registry.clone();
                let (cwd, _tmp) = tokio::task::spawn_blocking(move || state.state()).await??;
                crate::toolchain::resolve(&registry, &cwd, deadline).await
            })
            .await?;
        Some(resolved.clone())
    } else {
        None
    };
    let registry = config.registry.clone();
    let copy = key.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        registry.prepare(copy.language, &copy.workspace, copy.full, rust.as_ref())
    })
    .await??;
    Process::start(
        &prepared.command,
        Launch {
            root: key.workspace.clone(),
            language: key.language,
            settings: init::Settings::new(key.full, projects),
            request_timeout: config.request_timeout,
            typescript_engine: prepared.typescript_engine,
            python_interpreter: prepared.python_interpreter,
        },
    )
    .await
}

async fn receive_event(
    process: &mut Option<Process>,
) -> Option<Result<crate::protocol::Incoming, Error>> {
    match process {
        Some(process) => process.inbox.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
