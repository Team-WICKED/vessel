//! An `OpenAI`-compatible Chat Completions upstream (`OpenRouter`, Ollama,
//! vLLM, LM Studio, compatible endpoints of other providers): the neutral
//! request out as theirs, their stream in as neutral events.
//!
//! Compatible servers differ; what differs is a typed [`Options`] the node
//! fills from the provider's configuration, never guessed from a model's
//! name. What every server needs, learned from opencodex:
//! - one leading system message (strict chat templates refuse a system
//!   message later in the conversation);
//! - an assistant turn of only tool calls with `""` content, not `null`
//!   (strict validators refuse content-less entries);
//! - every tool call answered by a `tool` message right after it (the
//!   neutral form's [`ir::repair`] makes it so);
//! - tool call fragments assembled by index, then by id, with arguments
//!   that end empty becoming `{}`;
//! - the finish reason and the usage may come in separate chunks, and
//!   either may be missing; `[DONE]` or a finish reason ends the answer,
//!   and a stream with neither was cut short.
//!
//! Tool calls are held until the answer ends and then written whole, in
//! order: servers interleave the fragments of parallel calls, and a front
//! writes one call at a time. Text and reasoning stream as they come.

use crate::error::TranslateError;
use crate::ir;
use crate::sse::SseEvent;
use crate::wire::chat::{
    self as wire, AssistantMessage, Chunk, ContentPart, DONE, FinishReason, FunctionCall,
    FunctionCallBody, FunctionDefinition, FunctionTool, ImageUrl, ImageUrlPart, Message,
    NamedFunction, NamedToolChoice, PromptMessage, ReasoningConfig, ReasoningEffort, StreamOptions,
    ToolCall, ToolMessage,
};
use crate::wire::responses::{ToolChoiceMode, ToolName};
use crate::wire::{StrOr, TextOr, TextPart};

/// What a Chat Completions provider takes, from its configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The node's name for the provider, which reasoning is sealed with
    /// and checked against before it is sent back.
    pub provider: String,
    /// The provider takes `parallel_tool_calls`.
    pub parallel_tool_calls: bool,
    /// Which field bounds the output.
    pub max_tokens_field: MaxTokensField,
    /// How the provider is told how much to reason.
    pub reasoning: ReasoningStyle,
    /// The provider wants its reasoning back on assistant turns, as
    /// `reasoning_content` (some reasoning models lose their place in a
    /// tool loop without it; others refuse it).
    pub replay_reasoning: bool,
}

/// Which field bounds the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensField {
    /// `max_tokens`, which most compatible servers take.
    MaxTokens,
    /// `max_completion_tokens`, `OpenAI`'s newer name.
    MaxCompletionTokens,
}

/// How a provider is told how much to reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStyle {
    /// It is not told (the model's default).
    None,
    /// `reasoning_effort`, `OpenAI`'s way.
    Effort,
    /// `reasoning: { effort }`, `OpenRouter`'s way.
    OpenRouter,
}

/// The neutral request as a Chat Completions request, streamed.
///
/// # Errors
/// The request holds a PDF document, which these servers cannot take, or
/// a free-form tool's schema could not be written.
pub fn request(request: &ir::Request, options: &Options) -> Result<wire::Request, TranslateError> {
    let mut messages = Vec::with_capacity(request.messages.len().saturating_add(1));
    if !request.system.is_empty() {
        messages.push(Message::System(PromptMessage {
            content: TextOr::Text(request.system.join("\n\n")),
        }));
    }
    for message in &request.messages {
        match message.role {
            ir::Role::User => user(message, &mut messages)?,
            ir::Role::Assistant => assistant(message, options, &mut messages),
        }
    }

    let mut tools = Vec::with_capacity(request.tools.len());
    for tool in &request.tools {
        let (parameters, strict) = match &tool.kind {
            ir::ToolKind::Function { parameters, strict } => (parameters.clone(), *strict),
            ir::ToolKind::Freeform { grammar } => {
                (Some(ir::freeform_schema(grammar.as_ref())?), None)
            }
        };
        tools.push(wire::Tool::Function(FunctionTool {
            function: FunctionDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters,
                strict,
            },
        }));
    }
    let has_tools = !tools.is_empty();
    let tool_choice = has_tools.then(|| match &request.tool_choice {
        ir::ToolChoice::Auto => StrOr::Str(ToolChoiceMode::Auto),
        ir::ToolChoice::Any => StrOr::Str(ToolChoiceMode::Required),
        ir::ToolChoice::None => StrOr::Str(ToolChoiceMode::None),
        ir::ToolChoice::Tool(name) => StrOr::Object(NamedToolChoice::Function(NamedFunction {
            function: ToolName { name: name.clone() },
        })),
    });
    // On by default where the provider takes it (opencodex's experience:
    // the fragments are assembled safely); the request may still say no.
    let parallel_tool_calls = (has_tools && options.parallel_tool_calls)
        .then(|| request.parallel_tool_calls.unwrap_or(true));

    let effort = request.reasoning.effort().map(effort_of);
    let off = request.reasoning == ir::Reasoning::Off;
    let (reasoning_effort, reasoning) = match options.reasoning {
        ReasoningStyle::None => (None, None),
        ReasoningStyle::Effort => (
            if off {
                Some(ReasoningEffort::None)
            } else {
                effort
            },
            None,
        ),
        ReasoningStyle::OpenRouter => (
            None,
            (off || effort.is_some()).then_some(ReasoningConfig {
                effort: if off {
                    Some(ReasoningEffort::None)
                } else {
                    effort
                },
                max_tokens: None,
                exclude: None,
            }),
        ),
    };

    let (max_tokens, max_completion_tokens) = match options.max_tokens_field {
        MaxTokensField::MaxTokens => (request.max_output_tokens, None),
        MaxTokensField::MaxCompletionTokens => (None, request.max_output_tokens),
    };

    Ok(wire::Request {
        model: request.model.clone(),
        messages,
        tools,
        tool_choice,
        parallel_tool_calls,
        max_tokens,
        max_completion_tokens,
        temperature: request.sampling.temperature,
        top_p: request.sampling.top_p,
        stop: (!request.stop_sequences.is_empty())
            .then(|| TextOr::Parts(request.stop_sequences.clone())),
        stream: Some(true),
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        reasoning_effort,
        reasoning,
    })
}

const fn effort_of(effort: ir::Effort) -> ReasoningEffort {
    match effort {
        ir::Effort::Minimal => ReasoningEffort::Minimal,
        ir::Effort::Low => ReasoningEffort::Low,
        ir::Effort::Medium => ReasoningEffort::Medium,
        ir::Effort::High | ir::Effort::Max => ReasoningEffort::High,
    }
}

/// A user turn: its tool results as `tool` messages first (they must
/// follow the calls), then the rest as one user message.
fn user(message: &ir::Message, messages: &mut Vec<Message>) -> Result<(), TranslateError> {
    let mut parts = Vec::new();
    for part in &message.parts {
        match part {
            ir::Part::ToolResult(result) => {
                let mut text = result.text();
                if result.is_error {
                    text.insert_str(0, "[The tool failed.]\n");
                }
                messages.push(Message::Tool(ToolMessage {
                    tool_call_id: result.call_id.clone(),
                    content: TextOr::Text(text),
                }));
            }
            ir::Part::Text(text) => parts.push(ContentPart::Text(TextPart { text: text.clone() })),
            ir::Part::Image(image) => parts.push(image_part(image)),
            ir::Part::Document(ir::Document::Text { text, title }) => {
                let text = match title {
                    Some(title) => format!("[Document: {title}]\n{text}"),
                    None => text.clone(),
                };
                parts.push(ContentPart::Text(TextPart { text }));
            }
            ir::Part::Document(ir::Document::Pdf { .. }) => {
                return Err(TranslateError::Unsupported("a PDF document"));
            }
            // Not in a user turn.
            ir::Part::Thinking(_) | ir::Part::ToolCall(_) => {}
        }
    }
    if !parts.is_empty() {
        let content = match parts.as_slice() {
            [ContentPart::Text(only)] => TextOr::Text(only.text.clone()),
            _ => TextOr::Parts(parts),
        };
        messages.push(Message::User(PromptMessage { content }));
    }
    Ok(())
}

fn image_part(image: &ir::Image) -> ContentPart {
    ContentPart::ImageUrl(ImageUrlPart {
        image_url: ImageUrl {
            url: image.to_url().into_owned(),
            detail: None,
        },
    })
}

/// An assistant turn: its text, its tool calls, and its reasoning when
/// this provider made it and wants it back.
fn assistant(message: &ir::Message, options: &Options, messages: &mut Vec<Message>) {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for part in &message.parts {
        match part {
            ir::Part::Text(piece) => text.push_str(piece),
            ir::Part::Thinking(thinking) => {
                let ours = thinking.source.as_deref() == Some(options.provider.as_str())
                    && thinking.replay == ir::Replay::Text;
                if options.replay_reasoning && ours {
                    reasoning.push_str(&thinking.text);
                }
            }
            ir::Part::ToolCall(call) => tool_calls.push(ToolCall::Function(FunctionCall {
                id: if call.id.is_empty() {
                    ir::mint_call_id()
                } else {
                    call.id.clone()
                },
                function: FunctionCallBody {
                    name: call.name.clone(),
                    arguments: call.input.to_arguments().into_owned(),
                },
            })),
            // Not in an assistant turn.
            ir::Part::Image(_) | ir::Part::Document(_) | ir::Part::ToolResult(_) => {}
        }
    }
    if text.is_empty() && tool_calls.is_empty() && reasoning.is_empty() {
        return;
    }
    messages.push(Message::Assistant(AssistantMessage {
        content: Some(TextOr::Text(text)),
        tool_calls,
        reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
        reasoning: None,
    }));
}

/// A tool call being assembled.
#[derive(Debug)]
struct PendingCall {
    index: Option<u32>,
    id: String,
    name: String,
    arguments: String,
}

/// Reads a Chat Completions stream into neutral events.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    thinking: bool,
    calls: Vec<PendingCall>,
    finish: Option<FinishReason>,
    usage: Option<ir::Usage>,
    done: bool,
    ended: bool,
}

impl StreamDecoder {
    /// A decoder for one answer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the answer has ended (a failure was emitted, or
    /// [`finish`](Self::finish) ran).
    #[must_use]
    pub const fn ended(&self) -> bool {
        self.ended
    }

    /// Read one event of the stream.
    pub fn push(&mut self, event: &SseEvent, out: &mut Vec<ir::Event>) {
        if self.ended || self.done {
            return;
        }
        if event.data.trim() == DONE {
            self.done = true;
            return;
        }
        let chunk: Chunk = match serde_json::from_str(&event.data) {
            Ok(chunk) => chunk,
            Err(_unreadable) => {
                self.fail(
                    ir::Failure::new(
                        ir::FailureClass::UpstreamError,
                        "the provider sent a stream event Vessel cannot read",
                    ),
                    out,
                );
                return;
            }
        };
        if let Some(error) = chunk.error {
            self.fail(
                ir::Failure::new(ir::FailureClass::UpstreamError, error.message),
                out,
            );
            return;
        }
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage_of(&usage));
        }
        for choice in chunk.choices {
            if choice.index != 0 {
                continue;
            }
            let delta = choice.delta;
            let reasoning = delta.reasoning_content.or(delta.reasoning);
            if let Some(reasoning) = reasoning.filter(|text| !text.is_empty()) {
                self.thinking = true;
                out.push(ir::Event::Thinking(reasoning));
            }
            if let Some(content) = delta.content.filter(|text| !text.is_empty()) {
                self.end_thinking(out);
                out.push(ir::Event::Text(content));
            }
            if let Some(refusal) = delta.refusal.filter(|text| !text.is_empty()) {
                self.end_thinking(out);
                out.push(ir::Event::Text(refusal));
            }
            for piece in delta.tool_calls {
                self.end_thinking(out);
                self.add_piece(piece);
            }
            if let Some(finish) = choice.finish_reason {
                self.finish = Some(finish);
            }
        }
    }

    fn add_piece(&mut self, piece: wire::ToolCallDelta) {
        let (name, arguments) = piece
            .function
            .map(|function| (function.name, function.arguments))
            .unwrap_or_default();
        // By index; without one, by id; without either, the last call.
        let found = match (piece.index, piece.id.as_deref()) {
            (Some(index), _) => self.calls.iter().position(|call| call.index == Some(index)),
            (None, Some(id)) => self.calls.iter().position(|call| call.id == id),
            (None, None) => self.calls.len().checked_sub(1),
        };
        let at = found.unwrap_or_else(|| {
            self.calls.push(PendingCall {
                index: piece.index,
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            self.calls.len().saturating_sub(1)
        });
        let Some(call) = self.calls.get_mut(at) else {
            return;
        };
        if let Some(id) = piece.id.filter(|id| !id.is_empty()) {
            call.id = id;
        }
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            call.name = name;
        }
        if let Some(arguments) = arguments {
            call.arguments.push_str(&arguments);
        }
    }

    fn end_thinking(&mut self, out: &mut Vec<ir::Event>) {
        if self.thinking {
            self.thinking = false;
            out.push(ir::Event::ThinkingEnd(ir::Replay::Text));
        }
    }

    fn fail(&mut self, failure: ir::Failure, out: &mut Vec<ir::Event>) {
        self.ended = true;
        out.push(ir::Event::Failed(failure));
    }

    /// The stream ended: write the held tool calls, the usage and the
    /// stop, or a truncation if the stream never said it was done.
    pub fn finish(&mut self, out: &mut Vec<ir::Event>) {
        if self.ended {
            return;
        }
        self.end_thinking(out);
        if self.finish.is_none() && !self.done {
            self.fail(
                ir::Failure::new(
                    ir::FailureClass::Truncated,
                    "the provider's stream ended before the answer did",
                ),
                out,
            );
            return;
        }
        if self.finish == Some(FinishReason::Error) {
            self.fail(
                ir::Failure::new(
                    ir::FailureClass::UpstreamError,
                    "the provider ended the answer with an error",
                ),
                out,
            );
            return;
        }
        let had_calls = !self.calls.is_empty();
        self.calls.sort_by_key(|call| call.index);
        for call in std::mem::take(&mut self.calls) {
            out.push(ir::Event::ToolCallStart {
                id: if call.id.is_empty() {
                    ir::mint_call_id()
                } else {
                    call.id
                },
                name: call.name,
            });
            let arguments = if call.arguments.trim().is_empty() {
                String::from("{}")
            } else {
                call.arguments
            };
            out.push(ir::Event::ToolArgs(arguments));
            out.push(ir::Event::ToolCallEnd);
        }
        if let Some(usage) = self.usage {
            out.push(ir::Event::Usage(usage));
        }
        // A server that finished with calls but said `stop` still called
        // tools.
        let stop = if had_calls {
            ir::StopReason::ToolUse
        } else {
            match &self.finish {
                Some(FinishReason::Length) => ir::StopReason::MaxTokens,
                Some(FinishReason::ContentFilter) => ir::StopReason::Refusal,
                Some(
                    FinishReason::Stop
                    | FinishReason::ToolCalls
                    | FinishReason::FunctionCall
                    | FinishReason::Error
                    | FinishReason::Unknown(_),
                )
                | None => ir::StopReason::EndTurn,
            }
        };
        out.push(ir::Event::Stop(stop));
        self.ended = true;
    }
}

fn usage_of(usage: &wire::Usage) -> ir::Usage {
    let cached = usage
        .prompt_tokens_details
        .and_then(|details| details.cached_tokens)
        .unwrap_or(0);
    ir::Usage {
        input: usage.prompt_tokens.saturating_sub(cached),
        cache_read: cached,
        cache_write: 0,
        output: usage.completion_tokens,
        reasoning: usage
            .completion_tokens_details
            .and_then(|details| details.reasoning_tokens)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests;
