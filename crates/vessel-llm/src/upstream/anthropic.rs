//! An Anthropic Messages upstream, for a harness that speaks another API
//! (Codex on an Anthropic route): the neutral request out as an Anthropic
//! request, its stream in as neutral events. Claude Code on an Anthropic
//! route takes the passthrough instead.
//!
//! What Anthropic needs:
//! - `max_tokens` always, above the thinking budget;
//! - no sampling settings while thinking, and no forced tool choice;
//! - thinking blocks only its own (ADR 0015, decision 8): every other
//!   provider's reasoning, and every Vessel envelope, stays behind;
//! - tool call ids in its charset ([`ir::anthropic_safe_id`]), applied
//!   alike to a call and its result;
//! - text before tool calls in an assistant turn.
//!
//! Codex resends its long instructions and history every turn, so the
//! request marks two prompt-cache breakpoints: the end of the system
//! prompt and the end of the conversation.

use crate::error::TranslateError;
use crate::ir;
use crate::ir::envelope;
use crate::sse::SseEvent;
use crate::wire::anthropic::{
    self as wire, CacheControl, CacheKind, ContentBlock, CustomTool, Delta, DocumentBlock,
    DocumentSource, EncodedData, ErrorDetail, ErrorKind, ImageBlock, ImageSource, MessageParam,
    NamedToolChoice, OutputConfig, RedactedThinkingBlock, Role, StopReason, StreamEvent, TextBlock,
    ThinkingBlock, ThinkingBudget, ToolChoiceOptions, ToolResultBlock, ToolUseBlock, UrlSource,
};
use crate::wire::{Nothing, TextOr};
use vessel_proto::RawJson;

/// What an Anthropic provider takes, from its configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The node's name for the provider: only thinking it signed goes back
    /// to it.
    pub provider: String,
    /// The output bound when the harness gives none (Codex gives none).
    pub default_max_tokens: u32,
    /// The model thinks adaptively (`thinking: adaptive` with an effort)
    /// rather than with a budget.
    pub adaptive_thinking: bool,
}

/// The smallest thinking budget Anthropic takes.
const MIN_BUDGET: u32 = 1024;

/// The neutral request as an Anthropic request, streamed.
///
/// # Errors
/// A free-form tool's schema could not be written (it can).
pub fn request(request: &ir::Request, options: &Options) -> Result<wire::Request, TranslateError> {
    let mut max_tokens = request
        .max_output_tokens
        .unwrap_or(options.default_max_tokens);
    let (thinking, output_config) = match request.reasoning {
        ir::Reasoning::Default | ir::Reasoning::Off => (None, None),
        ir::Reasoning::Effort(effort) if options.adaptive_thinking => (
            Some(wire::Thinking::Adaptive(Nothing {})),
            Some(OutputConfig {
                effort: Some(match effort {
                    ir::Effort::Minimal | ir::Effort::Low => wire::Effort::Low,
                    ir::Effort::Medium => wire::Effort::Medium,
                    ir::Effort::High => wire::Effort::High,
                    ir::Effort::Max => wire::Effort::Max,
                }),
            }),
        ),
        ir::Reasoning::Effort(effort) => (
            Some(budget(effort.budget(Some(max_tokens)), &mut max_tokens)),
            None,
        ),
        ir::Reasoning::Budget(tokens) => (Some(budget(tokens, &mut max_tokens)), None),
    };
    let thinking_on = thinking.is_some();

    let mut messages: Vec<MessageParam> = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let (role, blocks) = match message.role {
            ir::Role::User => (Role::User, user(message)),
            ir::Role::Assistant => (Role::Assistant, assistant(message, options, thinking_on)),
        };
        if blocks.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some(MessageParam {
                role: last,
                content: TextOr::Parts(last_blocks),
            }) if *last == role => last_blocks.extend(blocks),
            Some(_) | None => messages.push(MessageParam {
                role,
                content: TextOr::Parts(blocks),
            }),
        }
    }
    mark_last(&mut messages);

    let tools = tools(&request.tools)?;
    let tool_choice = (!tools.is_empty()).then(|| tool_choice(request, thinking_on));

    let mut system: Vec<ContentBlock> = request
        .system
        .iter()
        .map(|text| ContentBlock::Text(text_block(text.clone())))
        .collect();
    if let Some(ContentBlock::Text(last)) = system.last_mut() {
        last.cache_control = Some(ephemeral());
    }

    // Sampling settings are refused while the model thinks.
    let sampling = (!thinking_on).then_some(request.sampling);
    Ok(wire::Request {
        model: request.model.clone(),
        messages,
        max_tokens: Some(max_tokens),
        system: (!system.is_empty()).then_some(TextOr::Parts(system)),
        tools,
        tool_choice,
        thinking,
        output_config,
        temperature: sampling.and_then(|sampling| sampling.temperature),
        top_p: sampling.and_then(|sampling| sampling.top_p),
        top_k: sampling.and_then(|sampling| sampling.top_k),
        stop_sequences: request.stop_sequences.clone(),
        stream: Some(true),
        metadata: None,
    })
}

fn tools(tools: &[ir::Tool]) -> Result<Vec<wire::Tool>, TranslateError> {
    let mut written = Vec::with_capacity(tools.len());
    for tool in tools {
        let input_schema = match &tool.kind {
            ir::ToolKind::Function { parameters, .. } => {
                parameters.clone().map_or_else(object_schema, Ok)?
            }
            ir::ToolKind::Freeform { grammar } => ir::freeform_schema(grammar.as_ref())?,
        };
        written.push(wire::Tool::Custom(CustomTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema,
            cache_control: None,
        }));
    }
    Ok(written)
}

fn tool_choice(request: &ir::Request, thinking_on: bool) -> wire::ToolChoice {
    let disable_parallel_tool_use = request.parallel_tool_calls.map(|parallel| !parallel);
    match &request.tool_choice {
        // Anthropic refuses a forced choice while the model thinks.
        ir::ToolChoice::Auto | ir::ToolChoice::Any | ir::ToolChoice::Tool(_) if thinking_on => {
            wire::ToolChoice::Auto(ToolChoiceOptions {
                disable_parallel_tool_use,
            })
        }
        ir::ToolChoice::Auto => wire::ToolChoice::Auto(ToolChoiceOptions {
            disable_parallel_tool_use,
        }),
        ir::ToolChoice::Any => wire::ToolChoice::Any(ToolChoiceOptions {
            disable_parallel_tool_use,
        }),
        ir::ToolChoice::Tool(name) => wire::ToolChoice::Tool(NamedToolChoice {
            name: name.clone(),
            disable_parallel_tool_use,
        }),
        ir::ToolChoice::None => wire::ToolChoice::None(Nothing {}),
    }
}

/// Thinking with `tokens` of budget, at least the smallest Anthropic
/// takes, with `max_tokens` raised above it if it was not.
fn budget(tokens: u32, max_tokens: &mut u32) -> wire::Thinking {
    let budget_tokens = tokens.max(MIN_BUDGET);
    if *max_tokens <= budget_tokens {
        *max_tokens = budget_tokens.saturating_add(MIN_BUDGET);
    }
    wire::Thinking::Enabled(ThinkingBudget { budget_tokens })
}

fn object_schema() -> Result<RawJson, TranslateError> {
    Ok(RawJson::from_text(r#"{"type":"object"}"#)?)
}

fn ephemeral() -> CacheControl {
    CacheControl {
        kind: CacheKind::Ephemeral,
        ttl: None,
    }
}

fn text_block(text: String) -> TextBlock {
    TextBlock {
        text,
        citations: None,
        cache_control: None,
    }
}

/// Put a cache breakpoint on the conversation's last block.
fn mark_last(messages: &mut [MessageParam]) {
    let Some(MessageParam {
        content: TextOr::Parts(blocks),
        ..
    }) = messages.last_mut()
    else {
        return;
    };
    match blocks.last_mut() {
        Some(ContentBlock::Text(block)) => block.cache_control = Some(ephemeral()),
        Some(ContentBlock::Image(block)) => block.cache_control = Some(ephemeral()),
        Some(ContentBlock::Document(block)) => block.cache_control = Some(ephemeral()),
        Some(ContentBlock::ToolUse(block)) => block.cache_control = Some(ephemeral()),
        Some(ContentBlock::ToolResult(block)) => block.cache_control = Some(ephemeral()),
        Some(
            ContentBlock::Thinking(_)
            | ContentBlock::RedactedThinking(_)
            | ContentBlock::Unknown { .. },
        )
        | None => {}
    }
}

fn user(message: &ir::Message) -> Vec<ContentBlock> {
    let mut blocks = Vec::with_capacity(message.parts.len());
    for part in &message.parts {
        match part {
            ir::Part::Text(text) => blocks.push(ContentBlock::Text(text_block(text.clone()))),
            ir::Part::Image(image) => blocks.push(image_block(image)),
            ir::Part::Document(document) => blocks.push(ContentBlock::Document(match document {
                ir::Document::Pdf { data, title } => DocumentBlock {
                    source: DocumentSource::Base64(EncodedData {
                        media_type: "application/pdf".to_owned(),
                        data: data.clone(),
                    }),
                    title: title.clone(),
                    context: None,
                    citations: None,
                    cache_control: None,
                },
                ir::Document::Text { text, title } => DocumentBlock {
                    source: DocumentSource::Text(EncodedData {
                        media_type: "text/plain".to_owned(),
                        data: text.clone(),
                    }),
                    title: title.clone(),
                    context: None,
                    citations: None,
                    cache_control: None,
                },
            })),
            ir::Part::ToolResult(result) => {
                let content = result
                    .content
                    .iter()
                    .map(|part| match part {
                        ir::ResultPart::Text(text) => ContentBlock::Text(text_block(text.clone())),
                        ir::ResultPart::Image(image) => image_block(image),
                    })
                    .collect();
                blocks.push(ContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: ir::anthropic_safe_id(&result.call_id).into_owned(),
                    content: Some(TextOr::Parts(content)),
                    is_error: result.is_error.then_some(true),
                    cache_control: None,
                }));
            }
            // Not in a user turn.
            ir::Part::Thinking(_) | ir::Part::ToolCall(_) => {}
        }
    }
    blocks
}

fn image_block(image: &ir::Image) -> ContentBlock {
    ContentBlock::Image(ImageBlock {
        source: match image {
            ir::Image::Base64 { media_type, data } => ImageSource::Base64(EncodedData {
                media_type: media_type.clone(),
                data: data.clone(),
            }),
            ir::Image::Url(url) => ImageSource::Url(UrlSource { url: url.clone() }),
        },
        cache_control: None,
    })
}

/// An assistant turn: thinking Anthropic signed (only while thinking),
/// then text, then tool calls.
fn assistant(message: &ir::Message, options: &Options, thinking_on: bool) -> Vec<ContentBlock> {
    let mut thinking = Vec::new();
    let mut text = Vec::new();
    let mut calls = Vec::new();
    for part in &message.parts {
        match part {
            ir::Part::Thinking(block) => {
                if let Some(block) = own_thinking(block, options).filter(|_| thinking_on) {
                    thinking.push(block);
                }
            }
            ir::Part::Text(piece) => {
                if !piece.is_empty() {
                    text.push(ContentBlock::Text(text_block(piece.clone())));
                }
            }
            ir::Part::ToolCall(call) => calls.push(ContentBlock::ToolUse(ToolUseBlock {
                id: ir::anthropic_safe_id(&call.id).into_owned(),
                name: call.name.clone(),
                input: arguments(call),
                cache_control: None,
            })),
            // Not in an assistant turn.
            ir::Part::Image(_) | ir::Part::Document(_) | ir::Part::ToolResult(_) => {}
        }
    }
    thinking.extend(text);
    thinking.extend(calls);
    thinking
}

/// A thinking block Anthropic made: natively signed (from no envelope) or
/// sealed by Vessel from this provider. Everything else, envelopes of
/// other providers included, never reaches Anthropic; nor does an opaque
/// field holding `vsl1:` anywhere (the last check before Anthropic, so no
/// path through an unwrapped envelope can send one).
fn own_thinking(thinking: &ir::Thinking, options: &Options) -> Option<ContentBlock> {
    let ours = thinking
        .source
        .as_deref()
        .is_none_or(|source| source == options.provider);
    if !ours {
        return None;
    }
    match &thinking.replay {
        ir::Replay::AnthropicSignature(signature)
            if !signature.is_empty() && !envelope::mentions(signature) =>
        {
            Some(ContentBlock::Thinking(ThinkingBlock {
                thinking: thinking.text.clone(),
                signature: signature.clone(),
            }))
        }
        ir::Replay::AnthropicRedacted(data) if !data.is_empty() && !envelope::mentions(data) => {
            Some(ContentBlock::RedactedThinking(RedactedThinkingBlock {
                data: data.clone(),
            }))
        }
        ir::Replay::AnthropicSignature(_)
        | ir::Replay::AnthropicRedacted(_)
        | ir::Replay::Text
        | ir::Replay::ResponsesEncrypted(_) => None,
    }
}

/// A call's arguments as the object Anthropic needs; arguments a model
/// wrote badly become `{}` rather than failing the whole history.
fn arguments(call: &ir::ToolCall) -> RawJson {
    let text = call.input.to_arguments();
    if text.trim_start().starts_with('{')
        && let Ok(object) = RawJson::from_text(text.into_owned())
    {
        return object;
    }
    RawJson::empty_object()
}

/// The block open in the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Open {
    None,
    Text,
    Thinking {
        signature: String,
    },
    Tool,
    /// A block Vessel does not know (a server tool's), ignored whole.
    Other,
}

/// Reads an Anthropic stream into neutral events.
#[derive(Debug)]
pub struct StreamDecoder {
    open: Open,
    usage: ir::Usage,
    stop: Option<ir::StopReason>,
    ended: bool,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    /// A decoder for one answer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            open: Open::None,
            usage: ir::Usage {
                input: 0,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
            stop: None,
            ended: false,
        }
    }

    /// Whether the answer has ended.
    #[must_use]
    pub const fn ended(&self) -> bool {
        self.ended
    }

    /// Read one event of the stream.
    pub fn push(&mut self, event: &SseEvent, out: &mut Vec<ir::Event>) {
        if self.ended {
            return;
        }
        let Ok(event) = serde_json::from_str::<StreamEvent>(&event.data) else {
            self.fail(
                ir::Failure::new(
                    ir::FailureClass::UpstreamError,
                    "the provider sent a stream event Vessel cannot read",
                ),
                out,
            );
            return;
        };
        match event {
            StreamEvent::MessageStart(start) => self.usage(&start.message.usage),
            StreamEvent::ContentBlockStart(start) => self.start(start.content_block, out),
            StreamEvent::ContentBlockDelta(delta) => self.delta(delta.delta, out),
            StreamEvent::ContentBlockStop(_) => {
                match std::mem::replace(&mut self.open, Open::None) {
                    Open::Thinking { signature } => {
                        out.push(ir::Event::ThinkingEnd(ir::Replay::AnthropicSignature(
                            signature,
                        )));
                    }
                    Open::Tool => out.push(ir::Event::ToolCallEnd),
                    Open::None | Open::Text | Open::Other => {}
                }
            }
            StreamEvent::MessageDelta(delta) => {
                self.usage(&delta.usage);
                self.stop = delta.delta.stop_reason.as_ref().map(stop_reason);
            }
            StreamEvent::MessageStop(Nothing {}) => {
                out.push(ir::Event::Usage(self.usage));
                out.push(ir::Event::Stop(
                    self.stop.unwrap_or(ir::StopReason::EndTurn),
                ));
                self.ended = true;
            }
            StreamEvent::Error(error) => self.fail(failure(&error.error), out),
            StreamEvent::Ping(Nothing {}) | StreamEvent::Unknown { .. } => {}
        }
    }

    /// Usage counts as they come: each present one replaces the last
    /// (`message_delta`'s are cumulative).
    fn usage(&mut self, usage: &wire::Usage) {
        if let Some(input) = usage.input_tokens {
            self.usage.input = input;
        }
        if let Some(read) = usage.cache_read_input_tokens {
            self.usage.cache_read = read;
        }
        if let Some(write) = usage.cache_creation_input_tokens {
            self.usage.cache_write = write;
        }
        if let Some(output) = usage.output_tokens {
            self.usage.output = output;
        }
    }

    fn start(&mut self, block: ContentBlock, out: &mut Vec<ir::Event>) {
        self.open = match block {
            ContentBlock::Text(block) => {
                if !block.text.is_empty() {
                    out.push(ir::Event::Text(block.text));
                }
                Open::Text
            }
            ContentBlock::Thinking(block) => {
                if !block.thinking.is_empty() {
                    out.push(ir::Event::Thinking(block.thinking));
                }
                Open::Thinking {
                    signature: block.signature,
                }
            }
            ContentBlock::RedactedThinking(block) => {
                out.push(ir::Event::ThinkingEnd(ir::Replay::AnthropicRedacted(
                    block.data,
                )));
                Open::Other
            }
            ContentBlock::ToolUse(call) => {
                out.push(ir::Event::ToolCallStart {
                    id: call.id,
                    name: call.name,
                });
                Open::Tool
            }
            ContentBlock::Image(_)
            | ContentBlock::Document(_)
            | ContentBlock::ToolResult(_)
            | ContentBlock::Unknown { .. } => Open::Other,
        };
    }

    fn delta(&mut self, delta: Delta, out: &mut Vec<ir::Event>) {
        match (&mut self.open, delta) {
            (Open::Text, Delta::Text(text)) => out.push(ir::Event::Text(text.text)),
            (Open::Thinking { .. }, Delta::Thinking(thinking)) => {
                out.push(ir::Event::Thinking(thinking.thinking));
            }
            (Open::Thinking { signature }, Delta::Signature(piece)) => {
                signature.push_str(&piece.signature);
            }
            (Open::Tool, Delta::InputJson(piece)) if !piece.partial_json.is_empty() => {
                out.push(ir::Event::ToolArgs(piece.partial_json));
            }
            _unmatched => {}
        }
    }

    fn fail(&mut self, failure: ir::Failure, out: &mut Vec<ir::Event>) {
        self.ended = true;
        out.push(ir::Event::Failed(failure));
    }

    /// The stream ended: a truncation unless `message_stop` came first.
    pub fn finish(&mut self, out: &mut Vec<ir::Event>) {
        if !self.ended {
            self.fail(
                ir::Failure::new(
                    ir::FailureClass::Truncated,
                    "the provider's stream ended before the message did",
                ),
                out,
            );
        }
    }
}

fn stop_reason(reason: &StopReason) -> ir::StopReason {
    match reason {
        StopReason::EndTurn | StopReason::Unknown(_) => ir::StopReason::EndTurn,
        StopReason::ToolUse => ir::StopReason::ToolUse,
        StopReason::MaxTokens | StopReason::ContextWindowExceeded => ir::StopReason::MaxTokens,
        StopReason::StopSequence => ir::StopReason::StopSequence,
        StopReason::Refusal => ir::StopReason::Refusal,
        StopReason::PauseTurn => ir::StopReason::PauseTurn,
    }
}

/// An Anthropic error as a neutral failure.
#[must_use]
pub fn failure(error: &ErrorDetail) -> ir::Failure {
    let class = match &error.kind {
        ErrorKind::RateLimit => ir::FailureClass::RateLimited,
        ErrorKind::Overloaded => ir::FailureClass::Overloaded,
        ErrorKind::Timeout => ir::FailureClass::Timeout,
        ErrorKind::Api | ErrorKind::Unknown(_) => ir::FailureClass::UpstreamError,
        ErrorKind::InvalidRequest if error.message.contains("prompt is too long") => {
            ir::FailureClass::ContextTooLong
        }
        ErrorKind::InvalidRequest | ErrorKind::NotFound | ErrorKind::RequestTooLarge => {
            ir::FailureClass::RefusedRequest
        }
        ErrorKind::Authentication | ErrorKind::Permission | ErrorKind::Billing => {
            ir::FailureClass::Unauthorized
        }
    };
    ir::Failure::new(class, error.message.clone())
}

#[cfg(test)]
mod tests;
