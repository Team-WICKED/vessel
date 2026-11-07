//! The tools, each one protocol call (or a call and a wait) through the
//! agent's own principal. Arguments and results are JSON; sessions, events
//! and nodes appear as the protocol describes them (PROTOCOL.md).
//!
//! [Decision Log]
//! - Other nodes (protocol 6, Track X): every tool but `list_nodes` takes
//!   `node`, a node's id or its name in `list_nodes` (`fleet.nodes`); a
//!   call for another node goes in `fleet.forward` (`NodeClient::call_on`),
//!   and that node applies its own grant, ownership and limits to the
//!   agent. Left out or naming this node, it is a call here. Names are
//!   chosen by each node, so a name more than one node has (this one and
//!   another included) is refused with the ids to choose from, never
//!   guessed (the review of Track X, item 5; the home also refuses a
//!   member named like another).
//! - Waiting is a long poll, here and elsewhere alike: `wait_turn` (and a
//!   new session's wait to be ready) is a loop of `session.read` with
//!   `wait`, so one code path serves every node and nothing needs a
//!   subscription across hops. A wait returns within one round trip of a
//!   record landing; an empty page means nothing happened in that wait.
//!   A node answers at once, empty, when too many reads already wait on
//!   the connection or for this agent (protocol 6); the loop then pauses
//!   [`EARLY_PAUSE`] before the next, so that is a poll a second, not a
//!   busy loop.

mod lsp;

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
use tokio::time::Instant;
use vessel_client::{CallError, NodeClient, Unary};
use vessel_proto::{
    AgentEvent, ContentBlock, CreateSession, EventPage, EventSeq, FleetNode, GetSession,
    HarnessKind, KillSession, ListNodes, ListSessions, ModelId, NodeId, PermissionOutcome,
    ReadEvents, SendInput, SessionEvent, SessionId, SessionInput, SessionMode, SessionSpec,
    SessionStatus, TerminalBytes, TerminalInput, TerminalSize,
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
/// One long poll's wait: under common 60 s idle limits on every hop.
const POLL_SECS: u32 = 30;
/// Records one poll reads.
const POLL_PAGE: u32 = 200;
/// The pause after a wait that came back empty before it could have ended
/// (the node had no room for it to wait).
const EARLY_PAUSE: Duration = Duration::from_secs(1);

/// The `vessel` tools over one connection to the agent's node.
#[derive(Debug, Clone)]
pub struct VesselTools {
    client: Arc<NodeClient>,
}

/// Where a tool's calls go: this node (`None`) or another of the fleet.
type Target = Option<NodeId>;

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
            name if name.starts_with("vessel_lsp_") => self.lsp(name, args).await,
            "list_nodes" => Ok(json!(self.client.call(ListNodes).await?)),
            "list_sessions" => {
                let node = self.target(args).await?;
                Ok(json!(self.on(node, ListSessions).await?))
            }
            "create_session" => self.create_session(args).await,
            "send_input" => self.send_input(args).await,
            "answer_permission" => self.answer_permission(args).await,
            "read_events" => self.read_events(args).await,
            "wait_turn" => self.wait_turn(args).await,
            "kill_session" => {
                let node = self.target(args).await?;
                let session = session(args)?;
                let _ack = self.on(node, KillSession { session }).await?;
                Ok(json!({ "killed": session }))
            }
            other => Err(ToolError::Arguments(format!("no tool `{other}`"))),
        }
    }

    /// The node a `node` argument names: this one when it is left out or
    /// names this node; another by its id, or by its name in the fleet.
    async fn target(&self, args: &JsonObject) -> Result<Target, ToolError> {
        let Some(given) = text(args, "node")? else {
            return Ok(None);
        };
        let this = self.client.node();
        if let Ok(id) = given.parse::<NodeId>() {
            return Ok((id != this.id).then_some(id));
        }
        let nodes = self.client.call(ListNodes).await?;
        resolve(given, this.id, &this.name, &nodes)
    }

    /// `request` on `node`: here, or forwarded there.
    async fn on<R: Unary>(&self, node: Target, request: R) -> Result<R::Response, ToolError> {
        Ok(match node {
            None => self.client.call(request).await?,
            Some(node) => self.client.call_on(node, request).await?,
        })
    }

    async fn create_session(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let node = self.target(args).await?;
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
        let info = self.on(node, CreateSession { spec }).await?;
        let Some(prompt) = text(args, "prompt")? else {
            return Ok(json!({ "session": info }));
        };
        if !terminal {
            self.until_running(node, info.id).await?;
        }
        let from = self.send_text(node, info.id, terminal, prompt).await?;
        let session = self.on(node, GetSession { session: info.id }).await?;
        Ok(json!({ "session": session, "from": from }))
    }

    /// One long poll of `session` from `from`, for at most `wait`.
    async fn poll(
        &self,
        node: Target,
        session: SessionId,
        from: EventSeq,
        wait: Duration,
    ) -> Result<EventPage, ToolError> {
        let seconds = u32::try_from(wait.as_secs())
            .unwrap_or(POLL_SECS)
            .clamp(1, POLL_SECS);
        self.on(
            node,
            ReadEvents {
                session,
                from,
                limit: POLL_PAGE,
                wait: Some(seconds),
            },
        )
        .await
    }

    /// Wait until a new structured session can take a prompt.
    async fn until_running(&self, node: Target, session: SessionId) -> Result<(), ToolError> {
        let deadline = after(READY_TIMEOUT);
        let mut next = EventSeq::ZERO;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ToolError::Failed(
                    "the session was not ready in time".to_owned(),
                ));
            }
            let asked = Instant::now();
            let page = self.poll(node, session, next, left).await?;
            pause_if_early(&page, asked, deadline).await;
            for record in &page.records {
                if let SessionEvent::Status { status } = &record.event {
                    if *status == SessionStatus::Running {
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
            next = page.next;
        }
    }

    async fn send_input(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let node = self.target(args).await?;
        let session = session(args)?;
        let text = required(args, "text")?;
        let terminal = matches!(
            self.on(node, GetSession { session }).await?.spec.mode,
            SessionMode::Terminal { .. }
        );
        let from = self.send_text(node, session, terminal, text).await?;
        Ok(json!({ "from": from }))
    }

    /// Send `text` as a prompt, or typed into a terminal followed by Enter;
    /// returns where the session's log stood just before.
    async fn send_text(
        &self,
        node: Target,
        session: SessionId,
        terminal: bool,
        text: &str,
    ) -> Result<EventSeq, ToolError> {
        let from = self.on(node, GetSession { session }).await?.next_seq;
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
        let _ack = self.on(node, SendInput { session, input }).await?;
        Ok(from)
    }

    async fn answer_permission(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let node = self.target(args).await?;
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
        let input = SessionInput::AnswerPermission { id, outcome };
        let _ack = self.on(node, SendInput { session, input }).await?;
        Ok(json!({ "answered": true }))
    }

    async fn read_events(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let node = self.target(args).await?;
        let session = session(args)?;
        let from = EventSeq::new(number(args, "from")?.unwrap_or(0));
        let limit = number(args, "limit")?
            .and_then(|limit| u32::try_from(limit).ok())
            .unwrap_or(READ_DEFAULT)
            .clamp(1, READ_MAX);
        let page = self
            .on(
                node,
                ReadEvents {
                    session,
                    from,
                    limit,
                    wait: None,
                },
            )
            .await?;
        Ok(json!(page))
    }

    /// Read the session from `from` until a turn ends, a permission is
    /// asked for, the session ends, or the wait runs out; returns the
    /// reply text read meanwhile and where to go on from.
    async fn wait_turn(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let node = self.target(args).await?;
        let session = session(args)?;
        // Required: from the start, the first turn ever would answer.
        let from = number(args, "from")?
            .map(EventSeq::new)
            .ok_or_else(|| ToolError::Arguments("`from` is required".to_owned()))?;
        let limit = number(args, "timeout_secs")?.map_or(WAIT_DEFAULT, |secs| {
            Duration::from_secs(secs.clamp(1, WAIT_MAX_SECS))
        });
        let deadline = after(limit);
        let mut reply = String::new();
        let mut next = from;
        let outcome = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break json!({ "outcome": "timeout" });
            }
            let asked = Instant::now();
            let page = self.poll(node, session, next, left).await?;
            pause_if_early(&page, asked, deadline).await;
            if let Some(outcome) = turn_outcome(&page, &mut reply, &mut next) {
                break outcome;
            }
            next = page.next;
            // Nothing arrived in a whole wait: an ended session has nothing
            // more to say (its final status may lie before `from`).
            if page.records.is_empty()
                && !self
                    .on(node, GetSession { session })
                    .await?
                    .status
                    .is_live()
            {
                break json!({ "outcome": "session_ended" });
            }
        };
        let mut result = outcome;
        if let Some(object) = result.as_object_mut() {
            let _none = object.insert("reply".to_owned(), Value::String(reply));
            let _none = object.insert("next".to_owned(), json!(next));
        }
        Ok(result)
    }
}

/// After an empty page that came back before a second had passed (the node
/// had no room for the read to wait), pause until a second has passed, or
/// until `deadline`.
async fn pause_if_early(page: &EventPage, asked: Instant, deadline: Instant) {
    if page.records.is_empty() {
        let until = asked
            .checked_add(EARLY_PAUSE)
            .unwrap_or(deadline)
            .min(deadline);
        tokio::time::sleep_until(until).await;
    }
}

/// The node named `given` (not an id): this one (`this`, named
/// `this_name`) or another of `nodes` (`fleet.nodes`). A name more than
/// one node has is refused with their ids, never guessed.
fn resolve(
    given: &str,
    this: NodeId,
    this_name: &str,
    nodes: &[FleetNode],
) -> Result<Target, ToolError> {
    let mut named: Vec<NodeId> = nodes
        .iter()
        .filter(|node| node.name == given)
        .map(|node| node.node)
        .collect();
    if this_name == given && !named.contains(&this) {
        named.push(this);
    }
    match named.as_slice() {
        [] => Err(ToolError::Arguments(format!(
            "no node `{given}` in this fleet (`list_nodes` shows the ones there are)"
        ))),
        [only] => Ok((*only != this).then_some(*only)),
        [_, _, ..] => {
            let ids: Vec<String> = named.iter().map(ToString::to_string).collect();
            Err(ToolError::Arguments(format!(
                "more than one node is named `{given}` ({}); give the id instead",
                ids.join(", ")
            )))
        }
    }
}

/// `wait` from now (now itself if that cannot be represented).
fn after(wait: Duration) -> Instant {
    Instant::now()
        .checked_add(wait)
        .unwrap_or_else(Instant::now)
}

/// What `page` says of a turn, if it ends one; the reply text read and
/// where the reading got to are kept in `reply` and `next`.
fn turn_outcome(page: &EventPage, reply: &mut String, next: &mut EventSeq) -> Option<Value> {
    for record in &page.records {
        *next = record.seq.next().unwrap_or(record.seq);
        match &record.event {
            SessionEvent::Agent {
                event:
                    AgentEvent::MessageChunk {
                        content: ContentBlock::Text { text },
                    },
            } => reply.push_str(text),
            SessionEvent::Agent {
                event: AgentEvent::TurnEnded { reason },
            } => return Some(json!({ "outcome": "ended", "reason": reason })),
            SessionEvent::Agent {
                event: AgentEvent::TurnFailed { message },
            } => return Some(json!({ "outcome": "failed", "message": message })),
            SessionEvent::Agent {
                event: AgentEvent::PermissionRequested(request),
            } => {
                return Some(json!({ "outcome": "permission_requested", "request": request }));
            }
            SessionEvent::Status { status } if !status.is_live() => {
                return Some(json!({ "outcome": "session_ended", "status": status }));
            }
            SessionEvent::Agent { .. }
            | SessionEvent::Status { .. }
            | SessionEvent::Terminal(_)
            | SessionEvent::ProfileChanged { .. } => {}
        }
    }
    None
}

impl ServerHandler for VesselTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("vessel", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Vessel: start and drive other agent sessions, on this machine or another node \
                 of its fleet (`node`, from `list_nodes`). A session you start is yours: you \
                 may send it input, wait for its turns, and stop it.",
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
            Err(ToolError::LspJson(error)) => {
                Err(ErrorData::invalid_params(error.to_string(), None))
            }
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
    /// Typed LSP arguments or results failed to encode/decode at the SDK edge.
    #[error("invalid LSP tool JSON")]
    LspJson(#[source] serde_json::Error),
    /// The node refused or failed; the agent reads why.
    #[error("{}", node_error(.0))]
    Node(#[from] CallError),
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
    session_tools().into_iter().chain(lsp::tools()).collect()
}

fn session_tools() -> Vec<Tool> {
    let session = json!({ "type": "string", "description": "A session id." });
    let node = json!({
        "type": "string",
        "description": "A node of the fleet, by its id or its name in `list_nodes`; leave it \
                        out for this node."
    });
    vec![
        tool(
            "list_nodes",
            "The nodes you can reach: this node and, in a fleet, the others (through the \
             fleet's home), with whether each is reachable now.",
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
            json!({ "node": node, "session": session, "text": { "type": "string" } }),
            &["session", "text"],
        ),
        tool(
            "answer_permission",
            "Answer a session's permission request (from `wait_turn`): the chosen option's \
             id, or none to refuse.",
            json!({
                "node": node,
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
                "node": node,
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
                "node": node,
                "session": session,
                "from": { "type": "integer", "minimum": 0 },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": WAIT_MAX_SECS }
            }),
            &["session", "from"],
        ),
        tool(
            "kill_session",
            "Stop a session you started (or one those started).",
            json!({ "node": node, "session": session }),
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

#[cfg(test)]
mod tests {
    use vessel_proto::PROTOCOL_VERSION;

    use super::*;

    fn node(id: NodeId, name: &str) -> FleetNode {
        FleetNode {
            node: id,
            name: name.to_owned(),
            protocol: PROTOCOL_VERSION,
            reachable: true,
        }
    }

    /// Names are each node's own choice: one that more than one node has is
    /// refused, whichever they are (the review of Track X, item 5).
    #[test]
    fn a_name_names_one_node_or_none() {
        let (here, desktop, laptop) = (NodeId::random(), NodeId::random(), NodeId::random());
        let fleet = [
            node(here, "home"),
            node(desktop, "desktop"),
            node(laptop, "laptop"),
        ];
        assert_eq!(resolve("home", here, "home", &fleet).unwrap(), None);
        assert_eq!(
            resolve("desktop", here, "home", &fleet).unwrap(),
            Some(desktop)
        );
        assert!(matches!(
            resolve("tablet", here, "home", &fleet),
            Err(ToolError::Arguments(_))
        ));
        // Outside a fleet `fleet.nodes` lists this node alone.
        assert_eq!(
            resolve("home", here, "home", &[node(here, "home")]).unwrap(),
            None
        );

        // Two members of one name, and another node named like this one.
        let twins = [
            node(here, "home"),
            node(desktop, "desktop"),
            node(laptop, "desktop"),
        ];
        let Err(ToolError::Arguments(message)) = resolve("desktop", here, "home", &twins) else {
            panic!("two nodes named alike were not refused");
        };
        assert!(
            message.contains(&desktop.to_string()) && message.contains(&laptop.to_string()),
            "{message}"
        );
        let mimic = [node(here, "home"), node(desktop, "home")];
        assert!(matches!(
            resolve("home", here, "home", &mimic),
            Err(ToolError::Arguments(_))
        ));
    }
}
