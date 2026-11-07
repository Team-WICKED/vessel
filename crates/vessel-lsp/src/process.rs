//! One LSP process and its bounded pipe tasks, owned by one server actor.
//!
//! One request at a time deliberately keeps saved-document versions and
//! responses ordered. Notifications and server requests remain serviced while
//! awaiting a response. A timeout poisons the connection; callers drop it and
//! restart, so late responses cannot be mistaken for another generation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use vessel_proc::{CommandSpec, Spawned};
use vessel_proto::lsp::{FilePath, Indexing, Language};

use crate::error::LspError as Error;
use crate::protocol::{self as rpc, data, initialize as init};

pub(crate) const MAX_DOCUMENTS: usize = 64;
pub(crate) const MAX_ITEMS: usize = 2000;

#[derive(Debug)]
pub(crate) struct Document {
    pub(crate) file: FilePath,
    pub(crate) path: PathBuf,
    pub(crate) text: std::sync::Arc<str>,
    pub(crate) version: i32,
    /// Last use on the process's LRU clock; the oldest is closed for room.
    pub(crate) used: u64,
}

/// What a server is started with, besides its command.
#[derive(Debug)]
pub(crate) struct Launch {
    pub(crate) root: PathBuf,
    pub(crate) language: Language,
    pub(crate) settings: init::Settings,
    pub(crate) request_timeout: Duration,
    pub(crate) typescript_engine: Option<PathBuf>,
    pub(crate) python_interpreter: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct Diagnostics {
    pub(crate) items: Vec<data::Diagnostic>,
    pub(crate) observed_at: DateTime<Utc>,
    pub(crate) version: Option<i32>,
}

#[derive(Debug)]
pub(crate) struct Process {
    child: vessel_proc::Child,
    pub(crate) input: tokio::process::ChildStdin,
    pub(crate) inbox: mpsc::Receiver<Result<rpc::Incoming, Error>>,
    reader: JoinHandle<()>,
    stderr: JoinHandle<()>,
    next_id: i64,
    pub(crate) root: PathBuf,
    pub(crate) language: Language,
    pub(crate) capabilities: init::Capabilities,
    pub(crate) documents: BTreeMap<String, Document>,
    pub(crate) diagnostics: BTreeMap<String, Diagnostics>,
    pub(crate) cache_truncated: bool,
    progress: BTreeSet<rpc::Id>,
    saw_progress: bool,
    rust_status: Option<init::RustStatus>,
    pub(crate) settings: init::Settings,
    pub(crate) python_interpreter: Option<PathBuf>,
    pub(crate) request_timeout: Duration,
    /// Monotonic use counter for document LRU. Saturating: 2^64 opens are
    /// out of reach, and a stuck clock only makes eviction order arbitrary.
    clock: u64,
}

impl Drop for Process {
    fn drop(&mut self) {
        // Aborting pumps closes pipes; Child's Drop terminates the entire
        // group/job even if initialization or a query was cancelled.
        self.reader.abort();
        self.stderr.abort();
    }
}

impl Process {
    pub(crate) async fn start(command: &CommandSpec, launch: Launch) -> Result<Self, Error> {
        let Launch {
            root,
            language,
            settings,
            request_timeout,
            typescript_engine,
            python_interpreter,
        } = launch;
        let Spawned {
            child,
            stdin,
            stdout,
            mut stderr,
        } = vessel_proc::spawn(command)?;
        // At most 8 frames of at most 4 MiB. Backpressure stops reading the
        // server until the actor can consume notifications/results.
        let (sender, inbox) = mpsc::channel(8);
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let item = match crate::framing::read(&mut reader).await {
                    Ok(Some(message)) => Ok(message),
                    Ok(None) => Err(Error::Protocol),
                    Err(error) => Err(error.into()),
                };
                let failed = item.is_err();
                if sender.send(item).await.is_err() || failed {
                    break;
                }
            }
        });
        let stderr = tokio::spawn(async move {
            // Server stderr may contain source text or environment values.
            // Drain into a fixed buffer without retaining or logging it.
            let mut buffer = [0_u8; 4096];
            while let Ok(count) = stderr.read(&mut buffer).await {
                if count == 0 {
                    break;
                }
            }
        });
        let mut process = Self {
            child,
            input: stdin,
            inbox,
            reader,
            stderr,
            next_id: 0,
            root,
            language,
            capabilities: init::Capabilities::default(),
            documents: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
            cache_truncated: false,
            progress: BTreeSet::new(),
            saw_progress: false,
            rust_status: None,
            settings,
            python_interpreter,
            request_timeout,
            clock: 0,
        };
        process.initialize(typescript_engine).await?;
        Ok(process)
    }

    async fn initialize(&mut self, typescript_engine: Option<PathBuf>) -> Result<(), Error> {
        let root_uri = crate::documents::uri(&self.root)?;
        let language = self.language;
        let mut capabilities = init::ClientCapabilities::default();
        capabilities.window.work_done_progress = true;
        if language == Language::Rust {
            capabilities.experimental = Some(init::Experimental {
                server_status_notification: true,
            });
        }
        capabilities.workspace.configuration = true;
        capabilities.workspace.folders = true;
        capabilities
            .text_document
            .document_symbol
            .hierarchical_document_symbol_support = true;
        let request = init::Initialize {
            process_id: std::process::id(),
            root_uri: root_uri.clone(),
            workspace_folders: vec![init::Folder {
                uri: root_uri,
                name: "workspace".to_owned(),
            }],
            capabilities,
            initialization_options: match language {
                Language::Rust => init::Options::Rust(self.settings.clone()),
                Language::TypeScript => init::Options::TypeScript(init::TypeScriptOptions {
                    disable_automatic_typing_acquisition: true,
                    plugins: [],
                    tsserver: init::TypeScriptServer {
                        path: typescript_engine,
                    },
                }),
                Language::Python | Language::Go | Language::Cpp => {
                    init::Options::Other(rpc::Empty {})
                }
            },
        };
        let initialized: init::Initialized = self.request("initialize", request).await?;
        if initialized
            .capabilities
            .position_encoding
            .as_deref()
            .is_some_and(|value| value != "utf-16")
        {
            return Err(Error::Unsupported);
        }
        self.capabilities = initialized.capabilities;
        self.notify("initialized", rpc::Empty {}).await?;
        if language == Language::Rust {
            // rust-analyzer's initial crate graph is itself an automatic
            // reload. Let this one preflighted load finish, then disable
            // background Cargo reloads before answering any user query.
            tokio::time::timeout(self.request_timeout, self.wait_initial_rust())
                .await
                .map_err(|_elapsed| Error::Timeout)??;
            self.settings.cargo.autoreload = false;
            self.notify(
                "workspace/didChangeConfiguration",
                init::ConfigurationChanged {
                    settings: init::RustSection {
                        rust: self.settings.clone(),
                    },
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn wait_initial_rust(&mut self) -> Result<(), Error> {
        loop {
            if let Some(status) = &self.rust_status {
                if matches!(status.health, init::Health::Error) {
                    return Err(Error::Unavailable);
                }
                if status.quiescent {
                    return Ok(());
                }
            }
            let message = self.inbox.recv().await.ok_or(Error::Protocol)??;
            self.event(message).await?;
        }
    }

    pub(crate) fn tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    pub(crate) fn indexing(&self) -> Indexing {
        if !self.progress.is_empty() {
            return Indexing::Busy;
        }
        if let Some(status) = &self.rust_status {
            return if status.quiescent {
                Indexing::ObservedIdle
            } else {
                Indexing::Busy
            };
        }
        if self.saw_progress {
            Indexing::ObservedIdle
        } else {
            Indexing::Unknown
        }
    }

    pub(crate) async fn notify<P: Serialize>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<(), Error> {
        let message = rpc::Notification {
            jsonrpc: "2.0",
            method,
            params,
        };
        tokio::time::timeout(
            self.request_timeout,
            crate::framing::write(&mut self.input, &message),
        )
        .await
        .map_err(|_elapsed| Error::Timeout)??;
        Ok(())
    }

    pub(crate) async fn request<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<R, Error> {
        tokio::time::timeout(self.request_timeout, self.request_inner(method, params))
            .await
            .map_err(|_elapsed| Error::Timeout)?
    }

    async fn request_inner<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<R, Error> {
        self.next_id = self.next_id.checked_add(1).ok_or(Error::Limit)?;
        let id = rpc::Id::Number(self.next_id);
        crate::framing::write(
            &mut self.input,
            &rpc::Request {
                jsonrpc: "2.0",
                id: id.clone(),
                method,
                params,
            },
        )
        .await?;
        loop {
            let message = self.inbox.recv().await.ok_or(Error::Protocol)??;
            if message.jsonrpc != "2.0" {
                return Err(Error::Protocol);
            }
            if message.method.is_none() {
                if message.id.as_ref() != Some(&id) {
                    return Err(Error::Protocol);
                }
                if let Some(error) = message.error {
                    return Err(Error::Server(error.code));
                }
                return Ok(serde_json::from_str(
                    message.result.as_ref().map_or("null", |raw| raw.get()),
                )?);
            }
            self.event(message).await?;
        }
    }

    pub(crate) async fn ready(&mut self) -> Result<(), Error> {
        while let Ok(message) = self.inbox.try_recv() {
            self.event(message?).await?;
        }
        // No standard barrier exists. Wait only for progress we observed,
        // then truthfully report Busy if a server continues indexing.
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_secs(2))
            .ok_or(Error::Limit)?;
        while matches!(self.indexing(), Indexing::Busy) {
            match tokio::time::timeout_at(deadline, self.inbox.recv()).await {
                Ok(Some(message)) => self.event(message?).await?,
                Ok(None) => return Err(Error::Protocol),
                Err(_elapsed) => break,
            }
        }
        Ok(())
    }

    pub(crate) async fn event(&mut self, message: rpc::Incoming) -> Result<(), Error> {
        if message.jsonrpc != "2.0" {
            return Err(Error::Protocol);
        }
        let params = message.params.as_ref().map_or("null", |raw| raw.get());
        let Some(method) = message.method.as_deref() else {
            return Err(Error::Protocol);
        };
        if let Some(id) = message.id {
            return self.server_request(id, method, params).await;
        }
        match method {
            "textDocument/publishDiagnostics" => {
                let report: data::Published = serde_json::from_str(params)?;
                self.cache(report).await?;
            }
            "experimental/serverStatus" if self.language == Language::Rust => {
                self.rust_status = Some(serde_json::from_str(params)?);
            }
            "$/progress" => {
                let progress: rpc::Progress = serde_json::from_str(params)?;
                if let rpc::Id::Text(token) = &progress.token
                    && token.len() > 1024
                {
                    return Err(Error::Limit);
                }
                self.saw_progress = true;
                match progress.value {
                    rpc::WorkDone::Begin => {
                        if self.progress.len() >= 64 {
                            return Err(Error::Limit);
                        }
                        let _inserted = self.progress.insert(progress.token);
                    }
                    rpc::WorkDone::End => {
                        let _removed = self.progress.remove(&progress.token);
                    }
                    rpc::WorkDone::Report => {}
                }
            }
            _ => {} // Open extension point: unknown server notifications are ignored.
        }
        Ok(())
    }

    pub(crate) async fn cache(&mut self, mut report: data::Published) -> Result<(), Error> {
        if crate::documents::from_uri(&self.root, &report.uri)
            .await
            .is_err()
        {
            self.cache_truncated = true;
            return Ok(());
        }
        if self.diagnostics.len() >= MAX_DOCUMENTS && !self.diagnostics.contains_key(&report.uri) {
            self.cache_truncated = true;
            return Ok(());
        }
        if let Some(document) = self.documents.get(&report.uri)
            && report
                .version
                .is_some_and(|version| version != document.version)
        {
            return Ok(());
        }
        let other_count = self
            .diagnostics
            .iter()
            .filter(|(uri, _)| **uri != report.uri)
            .map(|(_, report)| report.items.len())
            .fold(0_usize, usize::saturating_add);
        let room = MAX_ITEMS.saturating_sub(other_count);
        if report.diagnostics.len() > room {
            self.cache_truncated = true;
            report.diagnostics.truncate(room);
        }
        for diagnostic in &mut report.diagnostics {
            self.cache_truncated |= crate::query::truncate(&mut diagnostic.message, 8192);
            if let Some(source) = &mut diagnostic.source {
                self.cache_truncated |= crate::query::truncate(source, 256);
            }
        }
        drop(self.diagnostics.insert(
            report.uri,
            Diagnostics {
                items: report.diagnostics,
                observed_at: Utc::now(),
                version: report.version,
            },
        ));
        Ok(())
    }

    pub(crate) async fn shutdown(&mut self) {
        let shutdown = async {
            let _: () = self.request("shutdown", ()).await?;
            self.notify("exit", ()).await?;
            let _exit = self.child.wait().await?;
            Ok::<(), Error>(())
        };
        let _outcome = tokio::time::timeout(Duration::from_secs(2), shutdown).await;
    }
}
