//! The Anthropic Messages front, which Claude Code speaks: its request into
//! the neutral form, the neutral stream out as Anthropic's events, and
//! failures as Anthropic's error bodies.
//!
//! What Claude Code needs, learned from opencodex and the API's
//! documentation:
//! - one block open at a time, numbered from 0; a `message_start` first
//!   and a `message_stop` last, or an `error` event instead of the end;
//! - the cumulative usage, input included, in the final `message_delta`
//!   (the input is not known when an upstream other than Anthropic
//!   starts);
//! - tool call ids Anthropic would accept, so a session moved to an
//!   Anthropic route keeps a valid history ([`ir::anthropic_safe_id`]);
//! - reasoning from another provider as a thinking block whose signature
//!   is a Vessel envelope ([`envelope`]);
//! - a transient failure before the answer started as a 529
//!   `overloaded_error` with `Retry-After`, so Claude Code's own backoff
//!   takes over (ADR 0015, decision 11).

use std::time::Duration;

use crate::error::{Dropped, TranslateError};
use crate::ir::{self, envelope};
use crate::wire::anthropic::{
    self as wire, ContentBlock, ContentBlockDelta, ContentBlockStart, ContentBlockStop, Delta,
    ErrorBody, ErrorDetail, ErrorEvent, ErrorKind, ErrorTag, InputJsonDelta, Message, MessageDelta,
    MessageParam, MessageStart, MessageTag, Role, SignatureDelta, StopDelta, StopReason,
    StreamEvent, TextBlock, ThinkingBlock, ThinkingDelta, ToolUseBlock, Usage,
};
use crate::wire::{Nothing, TextOr, TextPart};
use vessel_proto::RawJson;

/// Claude Code's request in the neutral form, and what it held that the
/// neutral form cannot say (to log).
///
/// # Errors
/// The request holds something no provider can take through Vessel (an
/// image or a document by file id or URL, a non-text system block).
pub fn request(request: wire::Request) -> Result<(ir::Request, Vec<Dropped>), TranslateError> {
    let mut dropped = Vec::new();
    let mut system = Vec::new();
    match request.system {
        None => {}
        Some(TextOr::Text(text)) => system.push(text),
        Some(TextOr::Parts(blocks)) => {
            for block in blocks {
                system.push(system_text(block)?);
            }
        }
    }

    let mut messages = Vec::with_capacity(request.messages.len());
    for MessageParam { role, content } in request.messages {
        let blocks = match content {
            TextOr::Text(text) => vec![ContentBlock::Text(text_block(text))],
            TextOr::Parts(blocks) => blocks,
        };
        match role {
            // Claude Code's system messages join the system prompt: every
            // API takes one leading system prompt, few take more.
            Role::System => {
                for block in blocks {
                    system.push(system_text(block)?);
                }
            }
            Role::User | Role::Assistant => {
                let mut parts = Vec::with_capacity(blocks.len());
                for block in blocks {
                    if let Some(part) = part(block, &mut dropped)? {
                        parts.push(part);
                    }
                }
                messages.push(ir::Message {
                    role: if role == Role::User {
                        ir::Role::User
                    } else {
                        ir::Role::Assistant
                    },
                    parts,
                });
            }
        }
    }
    ir::repair(&mut messages);

    let tools = tools(request.tools, &mut dropped);
    let (tool_choice, parallel_tool_calls) = tool_choice(request.tool_choice, &mut dropped);
    let reasoning = reasoning(request.thinking, request.output_config, &mut dropped);

    Ok((
        ir::Request {
            model: request.model,
            system,
            messages,
            tools,
            tool_choice,
            parallel_tool_calls,
            max_output_tokens: request.max_tokens,
            sampling: ir::Sampling {
                temperature: request.temperature,
                top_p: request.top_p,
                top_k: request.top_k,
            },
            stop_sequences: request.stop_sequences,
            reasoning,
            cache_key: request
                .metadata
                .and_then(|metadata| metadata.user_id)
                .map(|user| ir::cache_key(&user)),
        },
        dropped,
    ))
}

fn tools(tools: Vec<wire::Tool>, dropped: &mut Vec<Dropped>) -> Vec<ir::Tool> {
    let mut neutral = Vec::with_capacity(tools.len());
    for tool in tools {
        match tool {
            wire::Tool::Custom(tool) => neutral.push(ir::Tool {
                name: tool.name,
                description: tool.description,
                kind: ir::ToolKind::Function {
                    parameters: Some(tool.input_schema),
                    strict: None,
                },
            }),
            // Server tools run at Anthropic; elsewhere there is nothing to
            // run them.
            wire::Tool::Unknown { kind, .. } => dropped.push(Dropped::new(kind, "tool")),
        }
    }
    neutral
}

fn tool_choice(
    choice: Option<wire::ToolChoice>,
    dropped: &mut Vec<Dropped>,
) -> (ir::ToolChoice, Option<bool>) {
    match choice {
        None => (ir::ToolChoice::Auto, None),
        Some(wire::ToolChoice::Auto(options)) => (ir::ToolChoice::Auto, parallel(&options)),
        Some(wire::ToolChoice::Any(options)) => (ir::ToolChoice::Any, parallel(&options)),
        Some(wire::ToolChoice::Tool(named)) => (
            ir::ToolChoice::Tool(named.name),
            named.disable_parallel_tool_use.map(|disable| !disable),
        ),
        Some(wire::ToolChoice::None(Nothing {})) => (ir::ToolChoice::None, None),
        Some(wire::ToolChoice::Unknown { kind, .. }) => {
            dropped.push(Dropped::new(kind, "tool choice"));
            (ir::ToolChoice::Auto, None)
        }
    }
}

/// Extended thinking: a budget, or adaptive with the effort in
/// `output_config` (Claude Code's `/effort`).
fn reasoning(
    thinking: Option<wire::Thinking>,
    output_config: Option<wire::OutputConfig>,
    dropped: &mut Vec<Dropped>,
) -> ir::Reasoning {
    let effort = output_config
        .and_then(|config| config.effort)
        .map(|effort| effort_of(&effort));
    match thinking {
        None => effort.map_or(ir::Reasoning::Default, ir::Reasoning::Effort),
        Some(wire::Thinking::Enabled(budget)) => ir::Reasoning::Budget(budget.budget_tokens),
        Some(wire::Thinking::Disabled(Nothing {})) => ir::Reasoning::Off,
        Some(wire::Thinking::Adaptive(Nothing {})) => {
            ir::Reasoning::Effort(effort.unwrap_or(ir::Effort::Medium))
        }
        Some(wire::Thinking::Unknown { kind, .. }) => {
            dropped.push(Dropped::new(kind, "thinking"));
            ir::Reasoning::Default
        }
    }
}

fn text_block(text: String) -> TextBlock {
    TextBlock {
        text,
        citations: None,
        cache_control: None,
    }
}

fn system_text(block: ContentBlock) -> Result<String, TranslateError> {
    match block {
        ContentBlock::Text(text) => Ok(text.text),
        ContentBlock::Image(_)
        | ContentBlock::Document(_)
        | ContentBlock::ToolUse(_)
        | ContentBlock::ToolResult(_)
        | ContentBlock::Thinking(_)
        | ContentBlock::RedactedThinking(_)
        | ContentBlock::Unknown { .. } => Err(TranslateError::Unsupported(
            "a system prompt block other than text",
        )),
    }
}

const fn parallel(options: &wire::ToolChoiceOptions) -> Option<bool> {
    match options.disable_parallel_tool_use {
        Some(disable) => Some(!disable),
        None => None,
    }
}

fn effort_of(effort: &wire::Effort) -> ir::Effort {
    match effort {
        wire::Effort::Low => ir::Effort::Low,
        wire::Effort::Medium => ir::Effort::Medium,
        wire::Effort::High | wire::Effort::Unknown(_) => ir::Effort::High,
        wire::Effort::Max => ir::Effort::Max,
    }
}

/// One content block of a user or assistant turn.
fn part(
    block: ContentBlock,
    dropped: &mut Vec<Dropped>,
) -> Result<Option<ir::Part>, TranslateError> {
    Ok(Some(match block {
        ContentBlock::Text(text) => ir::Part::Text(text.text),
        ContentBlock::Image(image) => ir::Part::Image(self::image(image.source)?),
        ContentBlock::Document(document) => {
            ir::Part::Document(self::document(document.source, document.title)?)
        }
        ContentBlock::ToolUse(call) => ir::Part::ToolCall(ir::ToolCall {
            id: call.id,
            name: call.name,
            input: ir::ToolInput::Json(call.input.as_text().to_owned()),
        }),
        ContentBlock::ToolResult(result) => {
            let content = match result.content {
                None => Vec::new(),
                Some(TextOr::Text(text)) => vec![ir::ResultPart::Text(text)],
                Some(TextOr::Parts(blocks)) => {
                    let mut content = Vec::with_capacity(blocks.len());
                    for block in blocks {
                        content.push(result_part(block, dropped)?);
                    }
                    content
                }
            };
            ir::Part::ToolResult(ir::ToolResult {
                call_id: result.tool_use_id,
                content,
                is_error: result.is_error.unwrap_or(false),
            })
        }
        ContentBlock::Thinking(thinking) => {
            ir::Part::Thinking(self::thinking(thinking.thinking, &thinking.signature))
        }
        ContentBlock::RedactedThinking(redacted) => ir::Part::Thinking(ir::Thinking {
            text: String::new(),
            replay: ir::Replay::AnthropicRedacted(redacted.data),
            source: None,
        }),
        ContentBlock::Unknown { kind, .. } => {
            dropped.push(Dropped::new(kind, "content block"));
            return Ok(None);
        }
    }))
}

/// A thinking block as Claude Code sends it back: Anthropic's own, or one
/// of Vessel's envelopes.
fn thinking(text: String, signature: &str) -> ir::Thinking {
    match envelope::open(signature) {
        Some((source, replay)) => ir::Thinking {
            text,
            replay,
            source: Some(source),
        },
        None if signature.is_empty() => ir::Thinking {
            text,
            replay: ir::Replay::Text,
            source: None,
        },
        None => ir::Thinking {
            text,
            replay: ir::Replay::AnthropicSignature(signature.to_owned()),
            source: None,
        },
    }
}

fn result_part(
    block: ContentBlock,
    dropped: &mut Vec<Dropped>,
) -> Result<ir::ResultPart, TranslateError> {
    Ok(match block {
        ContentBlock::Text(text) => ir::ResultPart::Text(text.text),
        ContentBlock::Image(image) => ir::ResultPart::Image(self::image(image.source)?),
        ContentBlock::Document(document) => {
            match self::document(document.source, document.title)? {
                ir::Document::Text { text, .. } => ir::ResultPart::Text(text),
                ir::Document::Pdf { .. } => {
                    dropped.push(Dropped::new("document", "tool result"));
                    ir::ResultPart::Text(DOCUMENT_MARKER.to_owned())
                }
            }
        }
        other @ (ContentBlock::ToolUse(_)
        | ContentBlock::ToolResult(_)
        | ContentBlock::Thinking(_)
        | ContentBlock::RedactedThinking(_)
        | ContentBlock::Unknown { .. }) => {
            dropped.push(Dropped::new(other.kind(), "tool result"));
            ir::ResultPart::Text(String::new())
        }
    })
}

/// What stands for a PDF inside a tool result.
const DOCUMENT_MARKER: &str =
    "[PDF document omitted: this route cannot pass documents in tool results]";

fn image(source: wire::ImageSource) -> Result<ir::Image, TranslateError> {
    match source {
        wire::ImageSource::Base64(data) => Ok(ir::Image::Base64 {
            media_type: data.media_type,
            data: data.data,
        }),
        wire::ImageSource::Url(url) => Ok(ir::Image::Url(url.url)),
        wire::ImageSource::File(_) | wire::ImageSource::Unknown { .. } => Err(
            TranslateError::Unsupported("an image by file id (files live at one provider)"),
        ),
    }
}

fn document(
    source: wire::DocumentSource,
    title: Option<String>,
) -> Result<ir::Document, TranslateError> {
    match source {
        wire::DocumentSource::Base64(data) => Ok(ir::Document::Pdf {
            data: data.data,
            title,
        }),
        wire::DocumentSource::Text(data) => Ok(ir::Document::Text {
            text: data.data,
            title,
        }),
        wire::DocumentSource::Content(content) => {
            let text = match content.content {
                TextOr::Text(text) => text,
                TextOr::Parts(blocks) => {
                    let mut text = String::new();
                    for block in blocks {
                        if let ContentBlock::Text(block) = block {
                            text.push_str(&block.text);
                        }
                    }
                    text
                }
            };
            Ok(ir::Document::Text { text, title })
        }
        wire::DocumentSource::Url(_)
        | wire::DocumentSource::File(_)
        | wire::DocumentSource::Unknown { .. } => Err(TranslateError::Unsupported(
            "a document by URL or file id (only inline documents travel)",
        )),
    }
}

/// The block open in the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Text,
    /// Whether its signature has been written.
    Thinking {
        signed: bool,
    },
    /// Whether any of its arguments have been written.
    Tool {
        has_args: bool,
    },
}

/// Writes the neutral stream as Anthropic's events, for one answer.
#[derive(Debug)]
pub struct StreamEncoder {
    id: String,
    model: String,
    /// The node's name for the provider, sealed into envelopes.
    source: String,
    started: bool,
    next_index: u32,
    open: Option<Open>,
    usage: ir::Usage,
    stop: Option<ir::StopReason>,
    ended: bool,
}

impl StreamEncoder {
    /// An encoder for an answer from `model`, made by the provider the
    /// node calls `source`.
    #[must_use]
    pub fn new(model: &str, source: &str) -> Self {
        Self {
            id: format!("msg_vsl_{}", uuid::Uuid::new_v4().simple()),
            model: model.to_owned(),
            source: source.to_owned(),
            started: false,
            next_index: 0,
            open: None,
            usage: ir::Usage::default(),
            stop: None,
            ended: false,
        }
    }

    /// Whether the answer has started (after which a failure is an `error`
    /// event, no longer an HTTP status).
    #[must_use]
    pub const fn started(&self) -> bool {
        self.started
    }

    /// Whether the stream has ended (`message_stop` or `error` written).
    #[must_use]
    pub const fn ended(&self) -> bool {
        self.ended
    }

    /// A keepalive, for a silence (Claude Code's own idle timeout must not
    /// fire while the provider thinks).
    #[must_use]
    pub fn ping() -> StreamEvent {
        StreamEvent::Ping(Nothing {})
    }

    /// Start the answer (`message_start`), if it has not started.
    pub fn start(&mut self, out: &mut Vec<StreamEvent>) {
        if self.started || self.ended {
            return;
        }
        self.started = true;
        out.push(StreamEvent::MessageStart(MessageStart {
            message: Message {
                id: self.id.clone(),
                tag: MessageTag::Message,
                role: Role::Assistant,
                model: self.model.clone(),
                content: Vec::new(),
                stop_reason: None,
                stop_sequence: None,
                usage: Usage {
                    input_tokens: Some(0),
                    output_tokens: Some(0),
                    ..Usage::default()
                },
            },
        }));
    }

    /// Write one event of the answer.
    pub fn push(&mut self, event: ir::Event, out: &mut Vec<StreamEvent>) {
        if self.ended {
            return;
        }
        match event {
            ir::Event::Text(text) => {
                self.enter_text(out);
                self.delta(Delta::Text(TextPart { text }), out);
            }
            ir::Event::Thinking(thinking) => {
                self.enter_thinking(out);
                self.delta(Delta::Thinking(ThinkingDelta { thinking }), out);
            }
            ir::Event::ThinkingEnd(replay) => {
                self.enter_thinking(out);
                let signature = envelope::seal(&self.source, &replay);
                self.delta(Delta::Signature(SignatureDelta { signature }), out);
                self.open = Some(Open::Thinking { signed: true });
                self.close(out);
            }
            ir::Event::ToolCallStart { id, name } => {
                self.close(out);
                self.start(out);
                self.open_block(
                    ContentBlock::ToolUse(ToolUseBlock {
                        id: ir::anthropic_safe_id(&id).into_owned(),
                        name,
                        input: RawJson::empty_object(),
                        cache_control: None,
                    }),
                    Open::Tool { has_args: false },
                    out,
                );
            }
            ir::Event::ToolArgs(partial_json) => {
                if let Some(Open::Tool { has_args }) = &mut self.open {
                    if partial_json.is_empty() {
                        return;
                    }
                    *has_args = true;
                    self.delta(Delta::InputJson(InputJsonDelta { partial_json }), out);
                }
            }
            ir::Event::ToolCallEnd => self.close(out),
            ir::Event::Usage(usage) => self.usage = usage,
            ir::Event::Stop(reason) => self.stop = Some(reason),
            ir::Event::Failed(failure) => self.fail(&failure, out),
        }
    }

    /// The upstream's stream ended: the answer ends properly if the model
    /// stopped, and as a truncation if not.
    pub fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        if self.ended {
            return;
        }
        let Some(stop) = self.stop else {
            self.fail(
                &ir::Failure::new(
                    ir::FailureClass::Truncated,
                    "the provider ended the answer without finishing it",
                ),
                out,
            );
            return;
        };
        self.start(out);
        self.close(out);
        out.push(StreamEvent::MessageDelta(MessageDelta {
            delta: StopDelta {
                stop_reason: Some(stop_reason(stop)),
                stop_sequence: None,
            },
            usage: Usage {
                input_tokens: Some(self.usage.input),
                cache_creation_input_tokens: Some(self.usage.cache_write),
                cache_read_input_tokens: Some(self.usage.cache_read),
                output_tokens: Some(self.usage.output),
            },
        }));
        out.push(StreamEvent::MessageStop(Nothing {}));
        self.ended = true;
    }

    fn fail(&mut self, failure: &ir::Failure, out: &mut Vec<StreamEvent>) {
        // An error event ends the stream where it is; the open block stays
        // open, as Anthropic's own streams do.
        out.push(StreamEvent::Error(ErrorEvent {
            error: error_detail(failure),
        }));
        self.ended = true;
    }

    /// Make a text block the open one, closing another first.
    fn enter_text(&mut self, out: &mut Vec<StreamEvent>) {
        if self.open == Some(Open::Text) {
            return;
        }
        self.close(out);
        self.start(out);
        self.open_block(
            ContentBlock::Text(text_block(String::new())),
            Open::Text,
            out,
        );
    }

    /// Make a thinking block the open one, closing another first.
    fn enter_thinking(&mut self, out: &mut Vec<StreamEvent>) {
        if self.open == Some(Open::Thinking { signed: false }) {
            return;
        }
        self.close(out);
        self.start(out);
        self.open_block(
            ContentBlock::Thinking(ThinkingBlock {
                thinking: String::new(),
                signature: String::new(),
            }),
            Open::Thinking { signed: false },
            out,
        );
    }

    fn open_block(&mut self, block: ContentBlock, kind: Open, out: &mut Vec<StreamEvent>) {
        out.push(StreamEvent::ContentBlockStart(ContentBlockStart {
            index: self.next_index,
            content_block: block,
        }));
        self.open = Some(kind);
    }

    fn delta(&self, delta: Delta, out: &mut Vec<StreamEvent>) {
        out.push(StreamEvent::ContentBlockDelta(ContentBlockDelta {
            index: self.next_index,
            delta,
        }));
    }

    fn close(&mut self, out: &mut Vec<StreamEvent>) {
        let Some(open) = self.open.take() else {
            return;
        };
        match open {
            // A thinking block without its signature: the provider's
            // reasoning had no replayable form, so seal its text alone.
            Open::Thinking { signed: false } => {
                let signature = envelope::seal(&self.source, &ir::Replay::Text);
                self.delta(Delta::Signature(SignatureDelta { signature }), out);
            }
            // Arguments that never came are `{}` (opencodex's lesson: a
            // tool without parameters gets none from some providers).
            Open::Tool { has_args: false } => {
                self.delta(
                    Delta::InputJson(InputJsonDelta {
                        partial_json: String::from("{}"),
                    }),
                    out,
                );
            }
            Open::Text | Open::Thinking { signed: true } | Open::Tool { has_args: true } => {}
        }
        out.push(StreamEvent::ContentBlockStop(ContentBlockStop {
            index: self.next_index,
        }));
        self.next_index = self.next_index.saturating_add(1);
    }
}

const fn stop_reason(stop: ir::StopReason) -> StopReason {
    match stop {
        ir::StopReason::EndTurn => StopReason::EndTurn,
        ir::StopReason::ToolUse => StopReason::ToolUse,
        ir::StopReason::MaxTokens => StopReason::MaxTokens,
        ir::StopReason::StopSequence => StopReason::StopSequence,
        ir::StopReason::Refusal => StopReason::Refusal,
        ir::StopReason::PauseTurn => StopReason::PauseTurn,
    }
}

fn error_detail(failure: &ir::Failure) -> ErrorDetail {
    let (kind, message) = match failure.class {
        ir::FailureClass::RateLimited
        | ir::FailureClass::Overloaded
        | ir::FailureClass::UpstreamError
        | ir::FailureClass::Timeout
        | ir::FailureClass::Truncated => (ErrorKind::Overloaded, failure.message.clone()),
        ir::FailureClass::RefusedRequest => (ErrorKind::InvalidRequest, failure.message.clone()),
        // Claude Code compacts the conversation on this wording.
        ir::FailureClass::ContextTooLong => (
            ErrorKind::InvalidRequest,
            format!("prompt is too long: {}", failure.message),
        ),
        ir::FailureClass::Unauthorized => (
            ErrorKind::Permission,
            format!(
                "the route's provider refused the node's key (check the node's models.toml and \
                 secrets.toml): {}",
                failure.message
            ),
        ),
    };
    ErrorDetail { kind, message }
}

/// How long Claude Code is told to wait after a transient failure the
/// provider gave no hint for.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);

/// A failure before the answer started, as an HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorResponse {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: ErrorBody,
    /// The `Retry-After` header, for a transient failure.
    pub retry_after: Option<Duration>,
}

/// `failure`, before the answer started, as Claude Code expects it: a
/// transient one as 529 with `Retry-After`, so its own backoff takes over.
#[must_use]
pub fn error_response(failure: &ir::Failure) -> ErrorResponse {
    let status = match failure.class {
        ir::FailureClass::RateLimited
        | ir::FailureClass::Overloaded
        | ir::FailureClass::UpstreamError
        | ir::FailureClass::Timeout
        | ir::FailureClass::Truncated => 529,
        ir::FailureClass::RefusedRequest | ir::FailureClass::ContextTooLong => 400,
        ir::FailureClass::Unauthorized => 403,
    };
    ErrorResponse {
        status,
        body: ErrorBody {
            tag: ErrorTag::Error,
            error: error_detail(failure),
        },
        retry_after: failure
            .class
            .is_transient()
            .then(|| failure.retry_after.unwrap_or(DEFAULT_RETRY_AFTER)),
    }
}

/// A request Vessel refuses before any provider sees it (it cannot be
/// translated, or it names another model), as a 400.
#[must_use]
pub fn refusal(message: String) -> ErrorResponse {
    ErrorResponse {
        status: 400,
        body: ErrorBody {
            tag: ErrorTag::Error,
            error: ErrorDetail {
                kind: ErrorKind::InvalidRequest,
                message,
            },
        },
        retry_after: None,
    }
}

/// Folds a stream's events into the whole message, for a caller that did
/// not ask for a stream.
#[derive(Debug, Default)]
pub struct Collector {
    message: Option<Message>,
    /// Each tool call's arguments as they arrive, by block index.
    arguments: Vec<(u32, String)>,
    failure: Option<ErrorDetail>,
}

impl Collector {
    /// Fold `event` in.
    pub fn push(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::MessageStart(start) => self.message = Some(start.message),
            StreamEvent::ContentBlockStart(start) => {
                if let Some(message) = &mut self.message {
                    message.content.push(start.content_block);
                }
            }
            StreamEvent::ContentBlockDelta(delta) => self.delta(delta),
            StreamEvent::MessageDelta(delta) => {
                if let Some(message) = &mut self.message {
                    message.stop_reason = delta.delta.stop_reason;
                    message.stop_sequence = delta.delta.stop_sequence;
                    message.usage = delta.usage;
                }
            }
            StreamEvent::Error(error) => self.failure = Some(error.error),
            StreamEvent::ContentBlockStop(_)
            | StreamEvent::MessageStop(_)
            | StreamEvent::Ping(_)
            | StreamEvent::Unknown { .. } => {}
        }
    }

    fn delta(&mut self, delta: ContentBlockDelta) {
        let Some(message) = &mut self.message else {
            return;
        };
        let Some(block) = message.content.last_mut() else {
            return;
        };
        match (block, delta.delta) {
            (ContentBlock::Text(block), Delta::Text(text)) => block.text.push_str(&text.text),
            (ContentBlock::Thinking(block), Delta::Thinking(thinking)) => {
                block.thinking.push_str(&thinking.thinking);
            }
            (ContentBlock::Thinking(block), Delta::Signature(signature)) => {
                block.signature.push_str(&signature.signature);
            }
            (ContentBlock::ToolUse(_), Delta::InputJson(piece)) => {
                match self
                    .arguments
                    .iter_mut()
                    .find(|(index, _)| *index == delta.index)
                {
                    Some((_, arguments)) => arguments.push_str(&piece.partial_json),
                    None => self.arguments.push((delta.index, piece.partial_json)),
                }
            }
            _other => {}
        }
    }

    /// The whole message, or the failure that ended it.
    ///
    /// # Errors
    /// The stream ended in an error event, never started, or holds tool
    /// arguments that are not JSON.
    pub fn finish(self) -> Result<Message, ErrorDetail> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        let Some(mut message) = self.message else {
            return Err(ErrorDetail {
                kind: ErrorKind::Api,
                message: "the answer never started".to_owned(),
            });
        };
        for (index, arguments) in self.arguments {
            let block = usize::try_from(index)
                .ok()
                .and_then(|index| message.content.get_mut(index));
            if let Some(ContentBlock::ToolUse(call)) = block {
                call.input = RawJson::from_text(arguments).map_err(|_invalid| ErrorDetail {
                    kind: ErrorKind::Api,
                    message: format!("tool call {} has arguments that are not JSON", call.id),
                })?;
            }
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests;
