//! The tools, each one protocol call (or a call and a wait) through the
//! agent's own principal. Arguments and results are JSON; sessions, events
//! and nodes appear as the protocol describes them (PROTOCOL.md).
//!
//! Other nodes come with fleet forwarding (plan, Track X): until then a
//! `node` argument must name this node, by id or name, or be left out.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock as McpContent,
    Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};
use vessel_client::{CallError, NodeClient, StreamError};
use vessel_proto::{
    AgentEvent, ContentBlock, EventSeq, HarnessKind, ModelId, PermissionOutcome, SessionEvent,
    SessionId, SessionInput, SessionMode, SessionSpec, SessionStatus, TerminalBytes, TerminalInput,
    TerminalSize,
};

/// How long `wait_turn` waits when not told.
const WAIT_DEFAULT: Duration = Duration::from_secs(600);
/// The longest `wait_turn` waits.
const WAIT_MAX_SECS: u64 = 3600;
/// How long a new structured session may take to be ready for its first
/// prompt (the node's own handshake limit is 60 s by default).
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Records `read_events` returns when not told, and at most.
const READ_DEFAULT: u32 = 100;
const READ_MAX: u32 = 1000;

/// The `vessel` tools over one connection to the agent's node.
#[derive(Debug, Clone)]
pub struct VesselTools {
    client: Arc<NodeClient>,
}

impl VesselTools {
    /// Tools calling through `client` (connected as the session's agent).
    #[must_use]
    pub fn new(client: NodeClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    async fn call(&self, name: &str, args: &JsonObject) -> Result<Value, ToolError> {
        match name {
            "list_nodes" => Ok(json!([self.client.node()])),
            "list_sessions" => {
                self.check_node(args)?;
                Ok(json!(self.client.sessions().await?))
            }
            "create_session" => self.create_session(args).await,
            "send_input" => self.send_input(args).await,
            "answer_permission" => self.answer_permission(args).await,
            "read_events" => self.read_events(args).await,
            "wait_turn" => self.wait_turn(args).await,
            "kill_session" => {
                let session = session(args)?;
                self.client.kill(session).await?;
                Ok(json!({ "killed": session }))
            }
            other => Err(ToolError::Arguments(format!("no tool `{other}`"))),
        }
    }

    /// A `node` argument, if any, must be this node (its id or name).
    fn check_node(&self, args: &JsonObject) -> Result<(), ToolError> {
        let Some(node) = text(args, "node")? else {
            return Ok(());
        };
        let this = self.client.node();
        if node == this.id.to_string() || node == this.name {
            return Ok(());
        }
        Err(ToolError::Arguments(format!(
            "`{node}` is not this node ({}); other nodes are not reachable from a session yet",
            this.name
        )))
    }

    async fn create_session(&self, args: &JsonObject) -> Result<Value, ToolError> {
        self.check_node(args)?;
        let harness: HarnessKind = required(args, "harness")?
            .parse()
            .map_err(|error| ToolError::Arguments(format!("{error}")))?;
        let terminal = flag(args, "terminal")?;
        let mode = if terminal {
            SessionMode::Terminal {
                args: Vec::new(),
                size: TerminalSize::default(),
            }
        } else {
            SessionMode::Structured {
                model: text(args, "model")?.map(ModelId::new),
                resume: None,
            }
        };
        let mut spec = SessionSpec::new(harness, mode);
        if let Some(cwd) = text(args, "cwd")? {
            spec = spec.with_cwd(PathBuf::from(cwd));
        }
        if let Some(profile) = text(args, "profile")? {
            let profile = profile
                .parse()
                .map_err(|error| ToolError::Arguments(format!("{error}")))?;
            spec = spec.with_profile(profile);
        }
        let info = self.client.create_session(spec).await?;
        let Some(prompt) = text(args, "prompt")? else {
            return Ok(json!({ "session": info }));
        };
        if !terminal {
            self.until_running(info.id).await?;
        }
        let from = self.send_text(info.id, terminal, prompt).await?;
        Ok(json!({ "session": self.client.session(info.id).await?, "from": from }))
    }

    /// Wait until a new structured session can take a prompt.
    async fn until_running(&self, session: SessionId) -> Result<(), ToolError> {
        let mut stream = self.client.follow(session, EventSeq::ZERO).await?;
        let waited = tokio::time::timeout(READY_TIMEOUT, async {
            while let Some(record) = stream.next().await {
                if let SessionEvent::Status { status } = record?.event {
                    if status == SessionStatus::Running {
                        return Ok(());
                    }
                    if !status.is_live() {
                        return Err(ToolError::Failed(format!(
                            "the session ended before it was ready: {}",
                            json!(status)
                        )));
                    }
                }
            }
            Err(ToolError::Failed("the session's log ended".to_owned()))
        })
        .await;
        waited.unwrap_or_else(|_elapsed| {
            Err(ToolError::Failed(
                "the session was not ready in time".to_owned(),
            ))
        })
    }

    async fn send_input(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let session = session(args)?;
        let text = required(args, "text")?;
        let terminal = matches!(
            self.client.session(session).await?.spec.mode,
            SessionMode::Terminal { .. }
        );
        let from = self.send_text(session, terminal, text).await?;
        Ok(json!({ "from": from }))
    }

    /// Send `text` as a prompt, or typed into a terminal followed by Enter;
    /// returns where the session's log stood just before.
    async fn send_text(
        &self,
        session: SessionId,
        terminal: bool,
        text: &str,
    ) -> Result<EventSeq, ToolError> {
        let from = self.client.session(session).await?.next_seq;
        let input = if terminal {
            SessionInput::Terminal {
                input: TerminalInput::Data {
                    data: TerminalBytes::new(Bytes::from(format!("{text}\r"))),
                },
            }
        } else {
            SessionInput::Prompt {
                content: vec![ContentBlock::Text {
                    text: text.to_owned(),
                }],
            }
        };
        self.client.send(session, input).await?;
        Ok(from)
    }

    async fn answer_permission(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let session = session(args)?;
        let id = serde_json::from_value(Value::String(required(args, "request")?.to_owned()))
            .map_err(|error| ToolError::Arguments(format!("request: {error}")))?;
        let outcome = match text(args, "option")? {
            Some(option) => PermissionOutcome::Selected {
                option: serde_json::from_value(Value::String(option.to_owned()))
                    .map_err(|error| ToolError::Arguments(format!("option: {error}")))?,
            },
            None => PermissionOutcome::Cancelled,
        };
        self.client
            .send(session, SessionInput::AnswerPermission { id, outcome })
            .await?;
        Ok(json!({ "answered": true }))
    }

    async fn read_events(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let session = session(args)?;
        let from = EventSeq::new(number(args, "from")?.unwrap_or(0));
        let limit = number(args, "limit")?
            .and_then(|limit| u32::try_from(limit).ok())
            .unwrap_or(READ_DEFAULT)
            .clamp(1, READ_MAX);
        Ok(json!(self.client.read(session, from, limit).await?))
    }

    /// Follow the session from `from` until a turn ends, a permission is
    /// asked for, the session ends, or the wait runs out; returns the
    /// reply text read meanwhile and where to go on from.
    async fn wait_turn(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let session = session(args)?;
        // Required: from the start, the first turn ever would answer.
        let from = number(args, "from")?
            .map(EventSeq::new)
            .ok_or_else(|| ToolError::Arguments("`from` is required".to_owned()))?;
        let limit = number(args, "timeout_secs")?.map_or(WAIT_DEFAULT, |secs| {
            Duration::from_secs(secs.clamp(1, WAIT_MAX_SECS))
        });
        let mut stream = self.client.follow(session, from).await?;
        let mut reply = String::new();
        let mut next = from;
        let waited = tokio::time::timeout(limit, async {
            while let Some(record) = stream.next().await {
                let record = record?;
                next = record.seq.next().unwrap_or(record.seq);
                match record.event {
                    SessionEvent::Agent {
                        event:
                            AgentEvent::MessageChunk {
                                content: ContentBlock::Text { text },
                            },
                    } => reply.push_str(&text),
                    SessionEvent::Agent {
                        event: AgentEvent::TurnEnded { reason },
                    } => return Ok(json!({ "outcome": "ended", "reason": reason })),
                    SessionEvent::Agent {
                        event: AgentEvent::TurnFailed { message },
                    } => return Ok(json!({ "outcome": "failed", "message": message })),
                    SessionEvent::Agent {
                        event: AgentEvent::PermissionRequested(request),
                    } => {
                        return Ok(
                            json!({ "outcome": "permission_requested", "request": request }),
                        );
                    }
                    SessionEvent::Status { status } if !status.is_live() => {
                        return Ok(json!({ "outcome": "session_ended", "status": status }));
                    }
                    SessionEvent::Agent { .. }
                    | SessionEvent::Status { .. }
                    | SessionEvent::Terminal(_)
                    | SessionEvent::ProfileChanged { .. } => {}
                }
            }
            Ok::<_, ToolError>(json!({ "outcome": "log_ended" }))
        })
        .await;
        let mut result = match waited {
            Ok(outcome) => outcome?,
            Err(_elapsed) => json!({ "outcome": "timeout" }),
        };
        if let Some(object) = result.as_object_mut() {
            let _none = object.insert("reply".to_owned(), Value::String(reply));
            let _none = object.insert("next".to_owned(), json!(next));
        }
        Ok(result)
    }
}

impl ServerHandler for VesselTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("vessel", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Vessel: start and drive other agent sessions on this machine. A session you \
                 start is yours: you may send it input, wait for its turns, and stop it.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        match self.call(&request.name, &args).await {
            Ok(value) => Ok(CallToolResult::success(vec![McpContent::text(pretty(&value))]).into()),
            Err(ToolError::Arguments(message)) => Err(ErrorData::invalid_params(message, None)),
            Err(refused) => {
                Ok(CallToolResult::error(vec![McpContent::text(refused.to_string())]).into())
            }
        }
    }
}

/// Why a tool call failed.
#[derive(Debug, thiserror::Error)]
enum ToolError {
    /// The call itself is wrong: a JSON-RPC error.
    #[error("{0}")]
    Arguments(String),
    /// The node refused or failed; the agent reads why.
    #[error("{}", node_error(.0))]
    Node(#[from] CallError),
    /// The session's log could not be followed.
    #[error("cannot follow the session: {0}")]
    Stream(#[from] StreamError),
    /// The tool could not do what it was asked.
    #[error("{0}")]
    Failed(String),
}

/// `code: message` for a refusal, the error's text otherwise.
fn node_error(error: &CallError) -> String {
    match error {
        CallError::Remote(error) => format!("{}: {}", json!(error.code), error.message),
        other @ (CallError::Disconnected
        | CallError::UnexpectedReply(_)
        | CallError::RequestIdsExhausted
        | CallError::NotInProtocol { .. }) => other.to_string(),
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_unprintable| value.to_string())
}

fn text<'a>(args: &'a JsonObject, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(ToolError::Arguments(format!("`{key}` must be a string"))),
    }
}

fn required<'a>(args: &'a JsonObject, key: &str) -> Result<&'a str, ToolError> {
    text(args, key)?.ok_or_else(|| ToolError::Arguments(format!("`{key}` is required")))
}

fn number(args: &JsonObject, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::Arguments(format!("`{key}` must be a non-negative integer"))),
    }
}

fn flag(args: &JsonObject, key: &str) -> Result<bool, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(ToolError::Arguments(format!(
            "`{key}` must be true or false"
        ))),
    }
}

fn session(args: &JsonObject) -> Result<SessionId, ToolError> {
    required(args, "session")?
        .parse()
        .map_err(|error| ToolError::Arguments(format!("session: {error}")))
}

/// Every tool with its input schema.
fn tools() -> Vec<Tool> {
    let session = json!({ "type": "string", "description": "A session id." });
    let node = json!({
        "type": "string",
        "description": "This node's id or name; leave it out for this node."
    });
    vec![
        tool(
            "list_nodes",
            "The nodes you can reach (this node, for now).",
            json!({}),
            &[],
        ),
        tool(
            "list_sessions",
            "Every session on the node, newest first: id, harness, mode, status, profile, and \
             who created it.",
            json!({ "node": node }),
            &[],
        ),
        tool(
            "create_session",
            "Start a session; it is yours to drive and stop. Structured (a conversation) unless \
             `terminal` is true. With `prompt`, it is sent once the session is ready; then call \
             `wait_turn` with the returned `from` for the reply.",
            json!({
                "node": node,
                "harness": {
                    "type": "string",
                    "description": "claude, codex, gemini or opencode."
                },
                "profile": {
                    "type": "string",
                    "description": "One of the node's profiles for that harness (an account)."
                },
                "cwd": {
                    "type": "string",
                    "description": "Absolute working directory on the node."
                },
                "model": { "type": "string", "description": "A model the agent offers." },
                "prompt": { "type": "string", "description": "The first prompt." },
                "terminal": {
                    "type": "boolean",
                    "description": "Run the harness's own terminal UI instead."
                }
            }),
            &["harness"],
        ),
        tool(
            "send_input",
            "Send a prompt to a structured session, or type a line into a terminal one. \
             Returns `from`, for `wait_turn`.",
            json!({ "session": session, "text": { "type": "string" } }),
            &["session", "text"],
        ),
        tool(
            "answer_permission",
            "Answer a session's permission request (from `wait_turn`): the chosen option's \
             id, or none to refuse.",
            json!({
                "session": session,
                "request": { "type": "string", "description": "The request's id." },
                "option": { "type": "string", "description": "The chosen option's id." }
            }),
            &["session", "request"],
        ),
        tool(
            "read_events",
            "A page of a session's log from record `from` (0 is the start).",
            json!({
                "session": session,
                "from": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1, "maximum": READ_MAX }
            }),
            &["session"],
        ),
        tool(
            "wait_turn",
            "Wait, from record `from` (what `send_input` or `create_session` returned, or a \
             previous `next`), until the session's turn ends, it asks for a permission, or it \
             ends. Returns the outcome, the reply text, and `next` to go on from.",
            json!({
                "session": session,
                "from": { "type": "integer", "minimum": 0 },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": WAIT_MAX_SECS }
            }),
            &["session", "from"],
        ),
        tool(
            "kill_session",
            "Stop a session you started (or one those started).",
            json!({ "session": session }),
            &["session"],
        ),
    ]
}

fn tool(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
) -> Tool {
    let mut schema = JsonObject::new();
    let _none = schema.insert("type".to_owned(), json!("object"));
    let _none = schema.insert("properties".to_owned(), properties);
    if !required.is_empty() {
        let _none = schema.insert("required".to_owned(), json!(required));
    }
    Tool::new(name, description, schema)
}
