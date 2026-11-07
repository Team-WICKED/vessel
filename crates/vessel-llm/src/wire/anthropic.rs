//! The Anthropic Messages API: `POST /v1/messages` (what Claude Code speaks,
//! and what an Anthropic upstream answers) and
//! `POST /v1/messages/count_tokens`.
//!
//! Claude Code also sends `role: "system"` messages inside `messages`
//! (opencodex learned this from real traffic), so [`Role`] has them.

use serde::{Deserialize, Serialize};
use vessel_proto::RawJson;

use super::open::{open_enum, open_str};
use super::{Nothing, TextOr, TextPart};

/// A request to `/v1/messages`, or to `/v1/messages/count_tokens` (the same
/// fields without `max_tokens` and `stream`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The model asked for.
    pub model: String,
    /// The conversation so far.
    pub messages: Vec<MessageParam>,
    /// The output bound (required by `/v1/messages`, absent from
    /// `count_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// The system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<TextOr<ContentBlock>>,
    /// Tools the model may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Whether and which tool the model must call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Extended thinking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    /// Output settings: the effort that goes with adaptive thinking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-k sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Strings that end the output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// Stream the answer as Server-Sent Events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// The caller's metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

/// A request's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// An opaque id of the caller (Claude Code puts its session in it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// One message of a request's conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageParam {
    /// Who said it.
    pub role: Role,
    /// What was said.
    pub content: TextOr<ContentBlock>,
}

/// Who said a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The user (and tool results).
    User,
    /// The model.
    Assistant,
    /// A system message inside the conversation (Claude Code sends them;
    /// the API itself takes the system prompt only in `system`).
    System,
}

open_enum! {
    /// A content block, in a request's messages, a response's content or a
    /// stream's `content_block_start`.
    pub enum ContentBlock in "type" {
        /// Text.
        "text" => Text(TextBlock),
        /// An image.
        "image" => Image(ImageBlock),
        /// A document (a PDF, plain text, or content blocks).
        "document" => Document(DocumentBlock),
        /// A call of a client tool.
        "tool_use" => ToolUse(ToolUseBlock),
        /// A client tool's result.
        "tool_result" => ToolResult(ToolResultBlock),
        /// The model's thinking, signed.
        "thinking" => Thinking(ThinkingBlock),
        /// Thinking the provider keeps encrypted.
        "redacted_thinking" => RedactedThinking(RedactedThinkingBlock),
    }
}

/// A text block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    /// The text.
    pub text: String,
    /// The text's citations (forwarded, never read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<RawJson>,
    /// A prompt-cache breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// An image block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBlock {
    /// Where the image comes from.
    pub source: ImageSource,
    /// A prompt-cache breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

open_enum! {
    /// Where an image comes from.
    pub enum ImageSource in "type" {
        /// Inline, base64-encoded.
        "base64" => Base64(EncodedData),
        /// A URL the provider fetches.
        "url" => Url(UrlSource),
        /// A file uploaded to the provider.
        "file" => File(FileSource),
    }
}

/// Inline data and its media type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodedData {
    /// Its MIME type (`image/png`, `application/pdf`, `text/plain`).
    pub media_type: String,
    /// The data: base64 for a `base64` source, the text itself for a `text`
    /// one.
    pub data: String,
}

/// A URL source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlSource {
    /// The URL.
    pub url: String,
}

/// An uploaded file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSource {
    /// The provider's file id.
    pub file_id: String,
}

/// A document block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentBlock {
    /// Where the document comes from.
    pub source: DocumentSource,
    /// Its title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Context about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Citation settings (forwarded, never read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<RawJson>,
    /// A prompt-cache breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

open_enum! {
    /// Where a document comes from.
    pub enum DocumentSource in "type" {
        /// A base64-encoded PDF.
        "base64" => Base64(EncodedData),
        /// Plain text.
        "text" => Text(EncodedData),
        /// Content blocks.
        "content" => Content(ContentSource),
        /// A URL the provider fetches.
        "url" => Url(UrlSource),
        /// A file uploaded to the provider.
        "file" => File(FileSource),
    }
}

/// A document made of content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentSource {
    /// The blocks.
    pub content: TextOr<ContentBlock>,
}

/// A call of a client tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUseBlock {
    /// The call's id (`toolu_...`), which its result names.
    pub id: String,
    /// The tool.
    pub name: String,
    /// The arguments (the agent's business: forwarded, never read).
    pub input: RawJson,
    /// A prompt-cache breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// A client tool's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    /// The call it answers.
    pub tool_use_id: String,
    /// What the tool returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<TextOr<ContentBlock>>,
    /// The call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// A prompt-cache breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// The model's thinking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// The thinking text.
    pub thinking: String,
    /// The provider's signature over it, opaque; empty until a stream's
    /// `signature_delta` fills it.
    pub signature: String,
}

/// Thinking the provider keeps encrypted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedThinkingBlock {
    /// The encrypted thinking, opaque.
    pub data: String,
}

/// A prompt-cache breakpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheControl {
    /// The kind of cache.
    #[serde(rename = "type")]
    pub kind: CacheKind,
    /// How long the entry lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CacheTtl>,
}

open_str! {
    /// The kind of a prompt cache.
    pub enum CacheKind {
        /// The only kind.
        "ephemeral" => Ephemeral,
    }
}

open_str! {
    /// How long a prompt-cache entry lives.
    pub enum CacheTtl {
        /// Five minutes (the default).
        "5m" => FiveMinutes,
        /// An hour.
        "1h" => OneHour,
    }
}

open_enum! {
    /// A tool offered to the model. Client tools may leave out their
    /// `type`; server tools (`web_search_20250305`, `bash_20250124`, ...)
    /// run at the provider and stay `Unknown` to Vessel.
    pub enum Tool in "type" or "custom" {
        /// A client tool: the harness runs it.
        "custom" => Custom(CustomTool),
    }
}

/// A client tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomTool {
    /// Its name.
    pub name: String,
    /// What it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The JSON Schema of its arguments (forwarded, never read).
    pub input_schema: RawJson,
    /// A prompt-cache breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

open_enum! {
    /// Whether and which tool the model must call.
    pub enum ToolChoice in "type" {
        /// The model decides.
        "auto" => Auto(ToolChoiceOptions),
        /// The model calls some tool.
        "any" => Any(ToolChoiceOptions),
        /// The model calls this tool.
        "tool" => Tool(NamedToolChoice),
        /// The model calls no tool.
        "none" => None(Nothing),
    }
}

/// A tool choice's option.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolChoiceOptions {
    /// At most one tool call per answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_parallel_tool_use: Option<bool>,
}

/// A choice of one tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedToolChoice {
    /// The tool.
    pub name: String,
    /// At most one tool call per answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_parallel_tool_use: Option<bool>,
}

open_enum! {
    /// Extended thinking.
    pub enum Thinking in "type" {
        /// Thinking with a token budget.
        "enabled" => Enabled(ThinkingBudget),
        /// No thinking.
        "disabled" => Disabled(Nothing),
        /// The model decides how much to think, guided by
        /// [`OutputConfig::effort`].
        "adaptive" => Adaptive(Nothing),
    }
}

/// A thinking budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingBudget {
    /// Tokens the model may think for; below the request's `max_tokens`.
    pub budget_tokens: u32,
}

/// Output settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputConfig {
    /// How hard the model works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

open_str! {
    /// How hard the model works.
    pub enum Effort {
        /// Low.
        "low" => Low,
        /// Medium.
        "medium" => Medium,
        /// High.
        "high" => High,
        /// The most.
        "max" => Max,
    }
}

/// A complete answer: the body of a non-streamed response, and the start
/// of a stream (with no content yet).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// The answer's id (`msg_...`).
    pub id: String,
    /// Always `message`.
    #[serde(rename = "type", default)]
    pub tag: MessageTag,
    /// Always the assistant.
    pub role: Role,
    /// The model that answered.
    pub model: String,
    /// What it said.
    pub content: Vec<ContentBlock>,
    /// Why it stopped (`null` while streaming).
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    /// The stop sequence it met, if that is why.
    #[serde(default)]
    pub stop_sequence: Option<String>,
    /// Tokens used.
    pub usage: Usage,
}

/// The constant `type` of a [`Message`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageTag {
    /// `message`.
    #[default]
    Message,
}

/// Tokens used. At a stream's start the counts so far; in its
/// `message_delta`, cumulative counts (replace, never add).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens neither read from nor written to the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Input tokens written to the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// Input tokens read from the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    /// Output tokens, thinking included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

open_str! {
    /// Why a model stopped.
    pub enum StopReason {
        /// It finished its turn.
        "end_turn" => EndTurn,
        /// It reached `max_tokens`.
        "max_tokens" => MaxTokens,
        /// It met a stop sequence.
        "stop_sequence" => StopSequence,
        /// It called a tool.
        "tool_use" => ToolUse,
        /// A long server-tool turn paused; send it back to continue.
        "pause_turn" => PauseTurn,
        /// It refused.
        "refusal" => Refusal,
        /// The conversation outgrew the model's context window.
        "model_context_window_exceeded" => ContextWindowExceeded,
    }
}

open_enum! {
    /// One event of a streamed answer.
    pub enum StreamEvent in "type" {
        /// The answer starts: its id, model and input usage.
        "message_start" => MessageStart(MessageStart),
        /// A content block starts.
        "content_block_start" => ContentBlockStart(ContentBlockStart),
        /// A content block grows.
        "content_block_delta" => ContentBlockDelta(ContentBlockDelta),
        /// A content block ends.
        "content_block_stop" => ContentBlockStop(ContentBlockStop),
        /// The stop reason and the cumulative usage.
        "message_delta" => MessageDelta(MessageDelta),
        /// The answer ends: the only proper end of a stream.
        "message_stop" => MessageStop(Nothing),
        /// A keepalive.
        "ping" => Ping(Nothing),
        /// The answer failed; the stream ends here.
        "error" => Error(ErrorEvent),
    }
}

/// `message_start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageStart {
    /// The answer so far.
    pub message: Message,
}

/// `content_block_start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlockStart {
    /// The block's place in the answer.
    pub index: u32,
    /// The block, empty.
    pub content_block: ContentBlock,
}

/// `content_block_delta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlockDelta {
    /// The block it grows.
    pub index: u32,
    /// What it adds.
    pub delta: Delta,
}

/// `content_block_stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentBlockStop {
    /// The block that ends.
    pub index: u32,
}

open_enum! {
    /// What a `content_block_delta` adds.
    pub enum Delta in "type" {
        /// Text.
        "text_delta" => Text(TextPart),
        /// A piece of a tool call's arguments, as JSON text.
        "input_json_delta" => InputJson(InputJsonDelta),
        /// Thinking text.
        "thinking_delta" => Thinking(ThinkingDelta),
        /// The thinking block's signature.
        "signature_delta" => Signature(SignatureDelta),
        /// A citation (forwarded, never read).
        "citations_delta" => Citations(CitationsDelta),
    }
}

/// A piece of a tool call's arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputJsonDelta {
    /// The piece, JSON text cut anywhere.
    pub partial_json: String,
}

/// Thinking text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingDelta {
    /// The text.
    pub thinking: String,
}

/// A thinking block's signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureDelta {
    /// The signature, opaque.
    pub signature: String,
}

/// A citation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationsDelta {
    /// The citation.
    pub citation: RawJson,
}

/// `message_delta`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDelta {
    /// The stop reason.
    pub delta: StopDelta,
    /// The usage so far (cumulative).
    pub usage: Usage,
}

/// The stop reason of a `message_delta`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopDelta {
    /// Why the model stopped.
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    /// The stop sequence it met.
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

/// An `error` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEvent {
    /// The error.
    pub error: ErrorDetail,
}

/// The body of an error response (any status but 200).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Always `error`.
    #[serde(rename = "type", default)]
    pub tag: ErrorTag,
    /// The error.
    pub error: ErrorDetail,
}

/// The constant `type` of an [`ErrorBody`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorTag {
    /// `error`.
    #[default]
    Error,
}

/// An error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// Its class.
    #[serde(rename = "type")]
    pub kind: ErrorKind,
    /// What went wrong, for a person.
    pub message: String,
}

open_str! {
    /// The class of an error.
    pub enum ErrorKind {
        /// The request is wrong (400).
        "invalid_request_error" => InvalidRequest,
        /// The key is wrong (401).
        "authentication_error" => Authentication,
        /// The account cannot pay (402).
        "billing_error" => Billing,
        /// The key may not do this (403).
        "permission_error" => Permission,
        /// No such thing (404).
        "not_found_error" => NotFound,
        /// The request is too large (413).
        "request_too_large" => RequestTooLarge,
        /// Too many requests (429).
        "rate_limit_error" => RateLimit,
        /// The provider failed (500).
        "api_error" => Api,
        /// The provider took too long (504).
        "timeout_error" => Timeout,
        /// The provider is overloaded (529); a client backs off and retries.
        "overloaded_error" => Overloaded,
    }
}

/// The answer of `/v1/messages/count_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCount {
    /// The request's input tokens.
    pub input_tokens: u64,
}

#[cfg(test)]
mod tests;
