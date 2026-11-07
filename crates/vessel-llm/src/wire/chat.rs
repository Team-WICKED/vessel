//! The `OpenAI`-compatible Chat Completions API: `POST /chat/completions`
//! under a provider's base URL (`OpenRouter`, Ollama, vLLM, LM Studio,
//! Gemini's compatible endpoint). The proxy only calls it, streaming, so
//! the request is written and the stream's chunks are read; a non-streamed
//! answer is not modelled.
//!
//! Compatible servers differ (opencodex's experience): reasoning comes as
//! `reasoning_content` (vLLM, `DeepSeek`) or `reasoning` (`OpenRouter`); a
//! tool call fragment may lack its `index`; the finish reason and the
//! usage may come in separate chunks; an error may arrive inside a 200
//! stream. Every such field is optional here and the translator decides.

use serde::{Deserialize, Serialize};
use vessel_proto::RawJson;

use super::open::{open_enum, open_str};
use super::{StrOr, TextOr, TextPart};

/// The sentinel `data:` of a stream's last event.
pub const DONE: &str = "[DONE]";

/// A request to `/chat/completions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The model asked for.
    pub model: String,
    /// The conversation so far, system prompt included.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Whether and which tool the model must call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Several tool calls per answer (not every server takes it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// The output bound, by its older name (most compatible servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// The output bound, by its newer name (`OpenAI` itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Strings that end the output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<TextOr<String>>,
    /// Stream the answer as Server-Sent Events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Streaming settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// How hard the model reasons, `OpenAI`'s way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// How hard the model reasons, `OpenRouter`'s way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
}

/// Streaming settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamOptions {
    /// End the stream with a chunk carrying the usage.
    pub include_usage: bool,
}

open_str! {
    /// How hard a model reasons.
    pub enum ReasoningEffort {
        /// Not at all.
        "none" => None,
        /// Barely.
        "minimal" => Minimal,
        /// Low.
        "low" => Low,
        /// Medium.
        "medium" => Medium,
        /// High.
        "high" => High,
    }
}

/// `OpenRouter`'s reasoning settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    /// How hard the model reasons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// A budget instead of an effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Reason, but leave the reasoning out of the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<bool>,
}

open_enum! {
    /// One message of the conversation, by its role.
    pub enum Message in "role" {
        /// The system prompt.
        "system" => System(PromptMessage),
        /// The developer's prompt (`OpenAI`'s newer name for the system).
        "developer" => Developer(PromptMessage),
        /// The user.
        "user" => User(PromptMessage),
        /// The model.
        "assistant" => Assistant(AssistantMessage),
        /// A tool's result.
        "tool" => Tool(ToolMessage),
    }
}

/// A system, developer or user message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptMessage {
    /// What was said.
    pub content: TextOr<ContentPart>,
}

/// A message from the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// What it said (`null` when it only called tools).
    #[serde(default)]
    pub content: Option<TextOr<ContentPart>>,
    /// The tools it called.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Its reasoning, sent back where a server takes it (vLLM, `DeepSeek`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Its reasoning, sent back the `OpenRouter` way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

/// A tool's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMessage {
    /// The call it answers.
    pub tool_call_id: String,
    /// What the tool returned.
    pub content: TextOr<ContentPart>,
}

open_enum! {
    /// A part of a message's content.
    #[derive(Eq)]
    pub enum ContentPart in "type" {
        /// Text.
        "text" => Text(TextPart),
        /// An image.
        "image_url" => ImageUrl(ImageUrlPart),
    }
}

/// An image part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrlPart {
    /// The image.
    pub image_url: ImageUrl,
}

/// An image by URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    /// A URL, or a `data:` URL with the image inline.
    pub url: String,
    /// How closely the model looks (`auto`, `low`, `high`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<super::responses::ImageDetail>,
}

open_enum! {
    /// A tool call in a message from the model.
    #[derive(Eq)]
    pub enum ToolCall in "type" {
        /// A function call.
        "function" => Function(FunctionCall),
    }
}

/// A function call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The call's id, which its result names.
    pub id: String,
    /// The function and its arguments.
    pub function: FunctionCallBody,
}

/// A called function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCallBody {
    /// The function.
    pub name: String,
    /// The arguments: JSON text in a string (forwarded, never read).
    pub arguments: String,
}

open_enum! {
    /// A tool offered to the model.
    pub enum Tool in "type" {
        /// A function.
        "function" => Function(FunctionTool),
    }
}

/// A function tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionTool {
    /// The function.
    pub function: FunctionDefinition,
}

/// A function the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDefinition {
    /// Its name.
    pub name: String,
    /// What it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The JSON Schema of its arguments (forwarded, never read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<RawJson>,
    /// Hold the model to the schema exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Whether and which tool the model must call: a mode or one function.
pub type ToolChoice = StrOr<super::responses::ToolChoiceMode, NamedToolChoice>;

open_enum! {
    /// A choice of one tool.
    #[derive(Eq)]
    pub enum NamedToolChoice in "type" {
        /// A function.
        "function" => Function(NamedFunction),
    }
}

/// A choice of one function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedFunction {
    /// The function.
    pub function: super::responses::ToolName,
}

/// One chunk of a streamed answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// The answer's id, the same in every chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The model that answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What this chunk adds (empty in a usage-only chunk).
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    /// Tokens used, in the last chunk when asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// An error inside the stream (`OpenRouter`): the stream ends here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorMessage>,
}

/// What a chunk adds to one choice (the proxy asks for one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkChoice {
    /// The choice.
    #[serde(default)]
    pub index: u32,
    /// What it adds.
    #[serde(default)]
    pub delta: ChunkDelta,
    /// Why the model stopped, in the chunk where it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

/// What a chunk adds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkDelta {
    /// Text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Reasoning (vLLM, `DeepSeek`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Reasoning (`OpenRouter`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// A refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// Pieces of tool calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// A piece of a tool call: the first piece of a call has its id and name,
/// the rest add to its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    /// The call's place among this answer's calls (some servers leave it
    /// out; the call's id then says which call it is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    /// The call's id, in its first piece.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The function's name and a piece of its arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionDelta>,
}

/// A piece of a function call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionDelta {
    /// The function, in the call's first piece.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A piece of the arguments, JSON text cut anywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

open_str! {
    /// Why a model stopped.
    pub enum FinishReason {
        /// It finished.
        "stop" => Stop,
        /// It reached the output bound.
        "length" => Length,
        /// It called tools.
        "tool_calls" => ToolCalls,
        /// A content filter stopped it.
        "content_filter" => ContentFilter,
        /// It called a function, the old way.
        "function_call" => FunctionCall,
        /// It failed (`OpenRouter`).
        "error" => Error,
    }
}

/// Tokens used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens, cached ones included.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Output tokens, reasoning included.
    #[serde(default)]
    pub completion_tokens: u64,
    /// The input tokens' breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// The output tokens' breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

/// The input tokens' breakdown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    /// Input tokens read from the prompt cache.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// The output tokens' breakdown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    /// Output tokens spent reasoning.
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

/// The body of an error response (any status but 200). Servers fill its
/// other fields differently (`code` is a string at `OpenAI`, a number at
/// `OpenRouter`), so only the message is read; the status says the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// The error.
    pub error: ErrorMessage,
}

/// An error, in an error body or inside a stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMessage {
    /// What went wrong, for a person.
    pub message: String,
}

#[cfg(test)]
mod tests;
