//! The neutral form every translation goes through: a request and a stream
//! of events that belong to no API.
//!
//! [Decision Log]
//! - Purpose: a harness speaks one API (its front) and a route's provider
//!   another (its upstream). Each pair must get thinking, tool calls,
//!   images, usage and stop reasons right.
//! - Constraint: two fronts and three upstreams in milestone 5, more
//!   later; information one API carries and another cannot (Anthropic's
//!   signatures, `OpenAI`'s encrypted reasoning, free-form tools) must
//!   survive a round trip through the harness where it can.
//! - Alternatives: one API as the hub (opencodex translates everything
//!   through Responses: an Anthropic-to-Anthropic call goes through it
//!   twice and loses what Responses cannot say); a translator per pair
//!   (N×M of them).
//! - Choice: this form, with one translator into it per front and one out
//!   of it per upstream (N+M), and a byte passthrough when both sides speak
//!   the same API (`crate::passthrough`). It holds what every API can say,
//!   plus [`Replay`]: a reasoning block's provider-specific form, carried
//!   opaquely so it can go back to the provider that made it and to no
//!   other.
//! - The stream is flat: text, thinking and tool calls follow one another,
//!   each ended by the next or by [`Event::ToolCallEnd`] /
//!   [`Event::ThinkingEnd`]; a front numbers its own blocks or items. An
//!   upstream emits exactly one terminal event, [`Event::Stop`] or
//!   [`Event::Failed`]; a stream that ends without one is a truncation.

use std::borrow::Cow;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use vessel_proto::RawJson;

pub mod envelope;

/// A model request in no API's terms.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The upstream model id (the route's; the node checked it).
    pub model: String,
    /// The system prompt, in parts (joined where an API takes one string).
    pub system: Vec<String>,
    /// The conversation, alternating between user and assistant turns.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<Tool>,
    /// Whether and which tool the model must call.
    pub tool_choice: ToolChoice,
    /// Several tool calls per answer: `None` leaves it to the provider.
    pub parallel_tool_calls: Option<bool>,
    /// The output bound.
    pub max_output_tokens: Option<u32>,
    /// Sampling settings.
    pub sampling: Sampling,
    /// Strings that end the output.
    pub stop_sequences: Vec<String>,
    /// How much the model reasons.
    pub reasoning: Reasoning,
    /// A key steering the provider's prompt-cache routing, if the caller
    /// gave something stable (already hashed: no caller id leaves as is).
    pub cache_key: Option<String>,
}

/// Sampling settings, each left to the provider when absent.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sampling {
    /// Temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    pub top_p: Option<f64>,
    /// Top-k sampling.
    pub top_k: Option<u32>,
}

/// How much a model reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reasoning {
    /// The caller did not say: the provider's default.
    Default,
    /// No reasoning.
    Off,
    /// An effort level.
    Effort(Effort),
    /// A token budget.
    Budget(u32),
}

impl Reasoning {
    /// The effort level this setting amounts to, for an API that takes
    /// levels (`None` for the default and for no reasoning). Budgets map
    /// as opencodex maps them: up to 4096 tokens is low, up to 16384
    /// medium, more is high.
    #[must_use]
    pub const fn effort(self) -> Option<Effort> {
        match self {
            Self::Default | Self::Off => None,
            Self::Effort(effort) => Some(effort),
            Self::Budget(budget) if budget <= 4096 => Some(Effort::Low),
            Self::Budget(budget) if budget <= 16_384 => Some(Effort::Medium),
            Self::Budget(_) => Some(Effort::High),
        }
    }
}

/// An effort level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    /// Barely.
    Minimal,
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// The most the model offers (Anthropic `max`, `OpenAI` `xhigh`).
    Max,
}

impl Effort {
    /// A thinking budget for this level, for an API that takes budgets,
    /// kept below `max_output_tokens` where that is known.
    #[must_use]
    pub fn budget(self, max_output_tokens: Option<u32>) -> u32 {
        let budget = match self {
            Self::Minimal => 1024,
            Self::Low => 4096,
            Self::Medium => 16_384,
            Self::High => 32_000,
            Self::Max => 64_000,
        };
        max_output_tokens.map_or(budget, |max| budget.min(max.saturating_sub(1).max(1024)))
    }
}

/// One turn of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Whose turn.
    pub role: Role,
    /// What it holds, in order.
    pub parts: Vec<Part>,
}

/// Whose turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The user's: text, images, documents, tool results.
    User,
    /// The model's: text, thinking, tool calls.
    Assistant,
}

/// A part of a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    /// Text.
    Text(String),
    /// An image.
    Image(Image),
    /// A document.
    Document(Document),
    /// The model's reasoning.
    Thinking(Thinking),
    /// A tool call.
    ToolCall(ToolCall),
    /// A tool's result.
    ToolResult(ToolResult),
}

/// An image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// Inline.
    Base64 {
        /// Its MIME type.
        media_type: String,
        /// The image, base64-encoded.
        data: String,
    },
    /// A URL the provider fetches (`data:` URLs are decoded into
    /// [`Image::Base64`] where they are recognised).
    Url(String),
}

impl Image {
    /// The image as a URL: itself, or a `data:` URL.
    #[must_use]
    pub fn to_url(&self) -> Cow<'_, str> {
        match self {
            Self::Base64 { media_type, data } => {
                Cow::Owned(format!("data:{media_type};base64,{data}"))
            }
            Self::Url(url) => Cow::Borrowed(url),
        }
    }

    /// An image from a URL, inline when it is a base64 `data:` URL.
    #[must_use]
    pub fn from_url(url: &str) -> Self {
        url.strip_prefix("data:")
            .and_then(|rest| rest.split_once(";base64,"))
            .map_or_else(
                || Self::Url(url.to_owned()),
                |(media_type, data)| Self::Base64 {
                    media_type: media_type.to_owned(),
                    data: data.to_owned(),
                },
            )
    }
}

/// A document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Document {
    /// A PDF, base64-encoded.
    Pdf {
        /// The PDF.
        data: String,
        /// Its title.
        title: Option<String>,
    },
    /// Plain text.
    Text {
        /// The text.
        text: String,
        /// Its title.
        title: Option<String>,
    },
}

/// The model's reasoning, and the form it can be replayed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thinking {
    /// The reasoning as text (a summary, or empty, where the provider shows
    /// none).
    pub text: String,
    /// The provider-specific form it goes back in.
    pub replay: Replay,
    /// The provider (by the node's name for it) that produced it, when it
    /// came back wrapped in a Vessel envelope; `None` when it came in the
    /// front's own native form.
    pub source: Option<String>,
}

/// The provider-specific form of a reasoning block. Each is accepted only
/// by the API that made it, and only from the provider that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replay {
    /// Nothing but the text.
    Text,
    /// Anthropic's signature over the text.
    AnthropicSignature(String),
    /// Anthropic's encrypted thinking (the text is empty).
    AnthropicRedacted(String),
    /// `OpenAI` Responses' encrypted reasoning.
    ResponsesEncrypted(String),
}

/// A tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Its id, which its result names; passed through unchanged except
    /// where an API restricts it ([`anthropic_safe_id`]).
    pub id: String,
    /// The tool.
    pub name: String,
    /// What it was called with.
    pub input: ToolInput,
}

/// What a tool was called with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolInput {
    /// JSON arguments, as text (never parsed here; possibly invalid when a
    /// model wrote them badly).
    Json(String),
    /// Free text, for a free-form tool.
    Text(String),
}

impl ToolInput {
    /// The input as JSON arguments: JSON stays (empty becomes `{}`), free
    /// text is wrapped as [`freeform_arguments`] does.
    #[must_use]
    pub fn to_arguments(&self) -> Cow<'_, str> {
        match self {
            Self::Json(json) if json.trim().is_empty() => Cow::Borrowed("{}"),
            Self::Json(json) => Cow::Borrowed(json),
            Self::Text(text) => Cow::Owned(freeform_arguments(text)),
        }
    }
}

/// A tool's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// The call it answers.
    pub call_id: String,
    /// What the tool returned.
    pub content: Vec<ResultPart>,
    /// The call failed.
    pub is_error: bool,
}

impl ToolResult {
    /// The result's text, images replaced by a marker, for an API whose
    /// tool results are text only.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::new();
        for part in &self.content {
            match part {
                ResultPart::Text(piece) => text.push_str(piece),
                ResultPart::Image(_) => text.push_str(IMAGE_MARKER),
            }
        }
        text
    }
}

/// What stands for an image in a tool result an upstream takes as text.
pub const IMAGE_MARKER: &str = "[image omitted: this model cannot take images in tool results]";

/// A part of a tool's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultPart {
    /// Text.
    Text(String),
    /// An image.
    Image(Image),
}

/// A tool offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    /// Its name.
    pub name: String,
    /// What it does.
    pub description: Option<String>,
    /// How it is called.
    pub kind: ToolKind,
}

/// How a tool is called.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolKind {
    /// With JSON arguments.
    Function {
        /// The arguments' JSON Schema (forwarded, never read).
        parameters: Option<RawJson>,
        /// Hold the model to the schema exactly.
        strict: Option<bool>,
    },
    /// With free text (Codex's `apply_patch`), which an API without
    /// free-form tools receives as a function taking one string
    /// ([`freeform_schema`]).
    Freeform {
        /// A grammar the text must follow, in its notation (described to
        /// the model where the API cannot enforce it).
        grammar: Option<(String, String)>,
    },
}

/// The JSON Schema of a free-form tool offered as a function: one string,
/// `input`.
///
/// # Errors
/// Never in practice: the schema is a struct of strings.
pub fn freeform_schema(grammar: Option<&(String, String)>) -> Result<RawJson, serde_json::Error> {
    let description = grammar.map_or_else(
        || "The tool's input, as free text.".to_owned(),
        |(syntax, definition)| {
            format!("The tool's input, as free text following this {syntax} grammar:\n{definition}")
        },
    );
    RawJson::from_serialize(&FreeformSchema {
        kind: "object",
        properties: FreeformProperties {
            input: FreeformInput {
                kind: "string",
                description,
            },
        },
        required: ["input"],
        additional_properties: false,
    })
}

#[derive(serde::Serialize)]
struct FreeformSchema {
    #[serde(rename = "type")]
    kind: &'static str,
    properties: FreeformProperties,
    required: [&'static str; 1],
    #[serde(rename = "additionalProperties")]
    additional_properties: bool,
}

#[derive(serde::Serialize)]
struct FreeformProperties {
    input: FreeformInput,
}

#[derive(serde::Serialize)]
struct FreeformInput {
    #[serde(rename = "type")]
    kind: &'static str,
    description: String,
}

/// A free-form tool's text as the arguments of its function form.
#[must_use]
pub fn freeform_arguments(text: &str) -> String {
    serde_json::to_string(&FreeformCall { input: text }).unwrap_or_else(|_never| String::from("{}"))
}

/// The text of a free-form tool's function-form arguments, if they are
/// that form.
#[must_use]
pub fn freeform_text(arguments: &str) -> Option<String> {
    serde_json::from_str::<FreeformCallOwned>(arguments)
        .ok()
        .map(|call| call.input)
}

#[derive(serde::Serialize)]
struct FreeformCall<'a> {
    input: &'a str,
}

#[derive(serde::Deserialize)]
struct FreeformCallOwned {
    input: String,
}

/// Whether and which tool the model must call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model decides.
    Auto,
    /// The model calls some tool.
    Any,
    /// The model calls this tool.
    Tool(String),
    /// The model calls no tool.
    None,
}

/// One event of a model's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Text.
    Text(String),
    /// Reasoning text.
    Thinking(String),
    /// The reasoning block ends, with the form it can be replayed in (a
    /// block may consist of this alone, when the provider shows no text).
    ThinkingEnd(Replay),
    /// A tool call starts.
    ToolCallStart {
        /// Its id.
        id: String,
        /// The tool.
        name: String,
    },
    /// A piece of the current tool call's arguments, JSON text cut
    /// anywhere.
    ToolArgs(String),
    /// The current tool call ends.
    ToolCallEnd,
    /// Tokens used so far (cumulative: each replaces the last).
    Usage(Usage),
    /// The model stopped: the proper end of the answer.
    Stop(StopReason),
    /// The answer failed: its end.
    Failed(Failure),
}

/// Tokens a call used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens neither read from nor written to a prompt cache.
    pub input: u64,
    /// Input tokens read from a prompt cache.
    pub cache_read: u64,
    /// Input tokens written to a prompt cache.
    pub cache_write: u64,
    /// Output tokens, reasoning included.
    pub output: u64,
    /// Of the output, tokens spent reasoning (where the provider says).
    pub reasoning: u64,
}

impl Usage {
    /// All input tokens, cached or not.
    #[must_use]
    pub const fn total_input(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

/// Why a model stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// It finished its turn.
    EndTurn,
    /// It called tools.
    ToolUse,
    /// It reached the output bound.
    MaxTokens,
    /// It met a stop sequence.
    StopSequence,
    /// It refused, or a content filter stopped it.
    Refusal,
    /// A long server-side turn paused (Anthropic).
    PauseTurn,
}

/// Why an answer failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Its class, which decides what the harness is told.
    pub class: FailureClass,
    /// What went wrong, for a person: never a key, already shortened.
    pub message: String,
    /// When to try again, if the provider said.
    pub retry_after: Option<Duration>,
}

impl Failure {
    /// A failure with no retry hint.
    #[must_use]
    pub fn new(class: FailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            retry_after: None,
        }
    }
}

/// The class of a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The provider limits the rate (429).
    RateLimited,
    /// The provider is overloaded (503, 529).
    Overloaded,
    /// The provider failed otherwise (5xx, an error inside the stream).
    UpstreamError,
    /// The provider refused the request as it is (4xx).
    RefusedRequest,
    /// The conversation outgrew the model's context window.
    ContextTooLong,
    /// The provider did not answer in time.
    Timeout,
    /// The stream ended without its end.
    Truncated,
    /// The provider refused the node's key.
    Unauthorized,
}

impl FailureClass {
    /// Whether the same request may succeed later: a harness should back
    /// off and retry.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        match self {
            Self::RateLimited
            | Self::Overloaded
            | Self::UpstreamError
            | Self::Timeout
            | Self::Truncated => true,
            Self::RefusedRequest | Self::ContextTooLong | Self::Unauthorized => false,
        }
    }
}

/// A tool call id as Anthropic accepts it (`[A-Za-z0-9_-]{1,64}`): the id
/// itself when it already is, otherwise a fixed mapping of it, so a call
/// and its result map alike. Other providers' ids (`functions.Write:0`)
/// would make Anthropic refuse the whole history.
#[must_use]
pub fn anthropic_safe_id(id: &str) -> Cow<'_, str> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        return Cow::Borrowed(id);
    }
    let digest = Sha256::digest(id.as_bytes());
    Cow::Owned(format!(
        "toolu_vsl_{}",
        hex(digest.get(..16).unwrap_or(&[]))
    ))
}

/// A fresh tool call id, for a provider that sent none: random, so unique
/// across the session (opencodex numbered them per stream, which collides
/// in a conversation's history).
#[must_use]
pub fn mint_call_id() -> String {
    format!("call_vsl_{}", uuid::Uuid::new_v4().simple())
}

/// A prompt-cache key from something stable the caller sent (Claude Code's
/// metadata, which holds its session), hashed so no caller id reaches a
/// provider as is.
#[must_use]
pub fn cache_key(stable: &str) -> String {
    let digest = Sha256::digest(stable.as_bytes());
    hex(digest.get(..16).unwrap_or(&[]))
}

/// `bytes` in lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        for nibble in [byte >> 4, byte & 0x0f] {
            text.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
    }
    text
}

/// Make the conversation one every API takes: turns alternate (adjacent
/// turns of one role are merged), every tool call is answered in the next
/// user turn (a missing result becomes an error result, and the results
/// come first in their turn), and a result whose call is nowhere before it
/// becomes text. A Codex history, or one cut by an interrupt, breaks each
/// of these at times.
pub fn repair(messages: &mut Vec<Message>) {
    let merged = merge_turns(std::mem::take(messages));
    let mut repaired: Vec<Message> = Vec::with_capacity(merged.len());
    let mut open_calls: Vec<String> = Vec::new();
    for message in merged {
        match message.role {
            Role::Assistant => {
                if !open_calls.is_empty() {
                    // The previous calls were never answered.
                    repaired.push(Message {
                        role: Role::User,
                        parts: missing_results(&mut open_calls),
                    });
                }
                open_calls = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::ToolCall(call) => Some(call.id.clone()),
                        Part::Text(_)
                        | Part::Image(_)
                        | Part::Document(_)
                        | Part::Thinking(_)
                        | Part::ToolResult(_) => None,
                    })
                    .collect();
                repaired.push(message);
            }
            Role::User => {
                let mut results = Vec::new();
                let mut rest = Vec::new();
                for part in message.parts {
                    match part {
                        Part::ToolResult(result) => {
                            if let Some(at) = open_calls.iter().position(|id| *id == result.call_id)
                            {
                                let _answered = open_calls.remove(at);
                                results.push(Part::ToolResult(result));
                            } else {
                                rest.push(Part::Text(orphan_text(&result)));
                            }
                        }
                        other @ (Part::Text(_)
                        | Part::Image(_)
                        | Part::Document(_)
                        | Part::Thinking(_)
                        | Part::ToolCall(_)) => rest.push(other),
                    }
                }
                results.extend(missing_results(&mut open_calls));
                results.extend(rest);
                if !results.is_empty() {
                    repaired.push(Message {
                        role: Role::User,
                        parts: results,
                    });
                }
            }
        }
    }
    // Calls left open at the very end are the ones the harness is about to
    // answer in its next request; a request never ends on them (it ends on
    // a user turn), so there is nothing to do here.
    *messages = merge_turns(repaired);
}

fn merge_turns(messages: Vec<Message>) -> Vec<Message> {
    let mut merged: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        if message.parts.is_empty() {
            continue;
        }
        match merged.last_mut() {
            Some(last) if last.role == message.role => last.parts.extend(message.parts),
            Some(_) | None => merged.push(message),
        }
    }
    merged
}

fn missing_results(open_calls: &mut Vec<String>) -> Vec<Part> {
    open_calls
        .drain(..)
        .map(|call_id| {
            Part::ToolResult(ToolResult {
                call_id,
                content: vec![ResultPart::Text(MISSING_RESULT.to_owned())],
                is_error: true,
            })
        })
        .collect()
}

/// The result given to a call the history never answered. The wording
/// claims nothing about what happened (opencodex's lesson: a model reads
/// "interrupted" as the user's answer).
pub const MISSING_RESULT: &str =
    "No result was recorded for this tool call; whether it ran is unknown.";

fn orphan_text(result: &ToolResult) -> String {
    format!(
        "[The result of tool call {}, whose call is not in this conversation:]\n{}",
        result.call_id,
        result.text()
    )
}

#[cfg(test)]
mod tests;
