//! The `OpenAI` Responses API: `POST /v1/responses` (what Codex speaks, and
//! what an `OpenAI` upstream answers).
//!
//! The items of a request's `input` and of a response's `output` are one
//! family ([`Item`]): Codex sends back what it was given. An input message
//! may leave out its `type` (opencodex learned this), so a missing tag
//! means `message`.

use serde::{Deserialize, Serialize};
use vessel_proto::RawJson;

use super::open::{open_enum, open_str};
use super::{Nothing, StrOr, TextOr, TextPart};

/// A request to `/v1/responses`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The model asked for.
    pub model: String,
    /// The system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The conversation so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<TextOr<Item>>,
    /// Tools the model may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Whether and which tool the model must call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Several tool calls per answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Reasoning settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// Keep the response at the provider (Codex sends `false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Stream the answer as Server-Sent Events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Extra output asked for (encrypted reasoning, so it can be replayed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<Include>,
    /// A key steering prompt-cache routing (Codex sends its conversation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Text output settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextConfig>,
    /// The output bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// A response kept at the provider to continue from (a translator,
    /// keeping nothing, refuses it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
}

open_enum! {
    /// An item of a conversation: input to a request, output of a response.
    pub enum Item in "type" or "message" {
        /// A message.
        "message" => Message(MessageItem),
        /// The model's reasoning.
        "reasoning" => Reasoning(ReasoningItem),
        /// A call of a function tool.
        "function_call" => FunctionCall(FunctionCallItem),
        /// A function tool's result.
        "function_call_output" => FunctionCallOutput(CallOutputItem),
        /// A call of a free-form tool (Codex's `apply_patch`).
        "custom_tool_call" => CustomToolCall(CustomToolCallItem),
        /// A free-form tool's result.
        "custom_tool_call_output" => CustomToolCallOutput(CallOutputItem),
    }
}

/// A message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageItem {
    /// Its id, in output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Who said it.
    pub role: Role,
    /// What was said.
    pub content: TextOr<ContentPart>,
    /// Whether it is finished, in output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

/// Who said a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The user.
    User,
    /// The model.
    Assistant,
    /// The system.
    System,
    /// The developer (Codex's sandbox and environment notes).
    Developer,
}

open_str! {
    /// Whether an item is finished.
    pub enum ItemStatus {
        /// Still growing.
        "in_progress" => InProgress,
        /// Done.
        "completed" => Completed,
        /// Cut short.
        "incomplete" => Incomplete,
    }
}

open_enum! {
    /// A part of a message's content.
    pub enum ContentPart in "type" {
        /// Text from the user.
        "input_text" => InputText(TextPart),
        /// An image from the user.
        "input_image" => InputImage(InputImage),
        /// A file from the user.
        "input_file" => InputFile(InputFile),
        /// Text from the model.
        "output_text" => OutputText(OutputText),
        /// The model's refusal.
        "refusal" => Refusal(Refusal),
    }
}

/// An image from the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputImage {
    /// A URL, or a `data:` URL with the image inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    /// A file uploaded to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    /// How closely the model looks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ImageDetail>,
}

open_str! {
    /// How closely a model looks at an image.
    pub enum ImageDetail {
        /// The model decides.
        "auto" => Auto,
        /// Low resolution.
        "low" => Low,
        /// High resolution.
        "high" => High,
    }
}

/// A file from the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFile {
    /// A file uploaded to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    /// The file inline, as a `data:` URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_data: Option<String>,
    /// Its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// A URL the provider fetches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_url: Option<String>,
}

/// Text from the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputText {
    /// The text.
    pub text: String,
    /// Its annotations (forwarded, never read; the API always writes the
    /// list).
    #[serde(default)]
    pub annotations: Vec<RawJson>,
}

/// The model's refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// Why it refused.
    pub refusal: String,
}

/// The model's reasoning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningItem {
    /// Its id (Codex leaves it out when sending reasoning back).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// A summary of it.
    #[serde(default)]
    pub summary: Vec<SummaryPart>,
    /// The reasoning itself, where the provider shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ReasoningPart>>,
    /// The reasoning, encrypted for the provider to read back; opaque.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    /// Whether it is finished, in output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

open_enum! {
    /// A part of a reasoning summary.
    #[derive(Eq)]
    pub enum SummaryPart in "type" {
        /// Text.
        "summary_text" => Text(TextPart),
    }
}

open_enum! {
    /// A part of reasoning shown in full.
    #[derive(Eq)]
    pub enum ReasoningPart in "type" {
        /// Text.
        "reasoning_text" => Text(TextPart),
    }
}

/// A call of a function tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCallItem {
    /// The item's id (`fc_...`), in output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The call's id (`call_...`), which its result names.
    pub call_id: String,
    /// The tool.
    pub name: String,
    /// The arguments: JSON text in a string, as the API sends it
    /// (forwarded, never read).
    pub arguments: String,
    /// Whether it is finished, in output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

/// A call of a free-form tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomToolCallItem {
    /// The item's id, in output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The call's id, which its result names.
    pub call_id: String,
    /// The tool.
    pub name: String,
    /// The input: free text (a patch, for `apply_patch`).
    pub input: String,
    /// Whether it is finished, in output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

/// A tool's result, function or free-form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallOutputItem {
    /// The item's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The call it answers.
    pub call_id: String,
    /// What the tool returned: text, or content parts (text and images).
    pub output: TextOr<ContentPart>,
}

open_enum! {
    /// A tool offered to the model. Hosted tools (`web_search`,
    /// `file_search`, ...) run at the provider and stay `Unknown` to
    /// Vessel.
    pub enum Tool in "type" {
        /// A function tool: JSON arguments.
        "function" => Function(FunctionTool),
        /// A free-form tool: text input, maybe constrained by a grammar.
        "custom" => Custom(CustomTool),
    }
}

/// A function tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionTool {
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

/// A free-form tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomTool {
    /// Its name.
    pub name: String,
    /// What it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What its input must look like.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<CustomFormat>,
}

open_enum! {
    /// What a free-form tool's input must look like.
    pub enum CustomFormat in "type" {
        /// Any text.
        "text" => Text(Nothing),
        /// Text a grammar accepts.
        "grammar" => Grammar(Grammar),
    }
}

/// A grammar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grammar {
    /// Its notation.
    pub syntax: GrammarSyntax,
    /// The grammar.
    pub definition: String,
}

open_str! {
    /// A grammar's notation.
    pub enum GrammarSyntax {
        /// Lark.
        "lark" => Lark,
        /// A regular expression.
        "regex" => Regex,
    }
}

/// Whether and which tool the model must call: a mode or one tool.
pub type ToolChoice = StrOr<ToolChoiceMode, NamedToolChoice>;

open_str! {
    /// A tool choice by mode.
    pub enum ToolChoiceMode {
        /// The model decides.
        "auto" => Auto,
        /// The model calls no tool.
        "none" => None,
        /// The model calls some tool.
        "required" => Required,
    }
}

open_enum! {
    /// A choice of one tool.
    pub enum NamedToolChoice in "type" {
        /// A function tool.
        "function" => Function(ToolName),
        /// A free-form tool.
        "custom" => Custom(ToolName),
    }
}

/// A tool's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolName {
    /// The name.
    pub name: String,
}

/// Reasoning settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reasoning {
    /// How hard the model reasons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// Whether and how the reasoning is summarised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummary>,
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
        /// Higher.
        "xhigh" => ExtraHigh,
    }
}

open_str! {
    /// How reasoning is summarised.
    pub enum ReasoningSummary {
        /// The model decides.
        "auto" => Auto,
        /// Briefly.
        "concise" => Concise,
        /// At length.
        "detailed" => Detailed,
        /// Not at all.
        "none" => None,
    }
}

open_str! {
    /// Extra output a request asks for.
    pub enum Include {
        /// Reasoning, encrypted, so a stateless caller can send it back.
        "reasoning.encrypted_content" => ReasoningEncryptedContent,
    }
}

/// Text output settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextConfig {
    /// How much the model writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
    /// A structured output format (forwarded, never read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<RawJson>,
}

open_str! {
    /// How much a model writes.
    pub enum Verbosity {
        /// Little.
        "low" => Low,
        /// Some.
        "medium" => Medium,
        /// Much.
        "high" => High,
    }
}

/// A response: what the stream's `response.*` events carry, and the body
/// of a non-streamed answer. Only the fields a caller reads are modelled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Its id (`resp_...`).
    pub id: String,
    /// Always `response`.
    #[serde(default)]
    pub object: ResponseTag,
    /// Where it is.
    pub status: ResponseStatus,
    /// The model that answered.
    pub model: String,
    /// What it produced (the stream's `response.completed` may leave this
    /// empty; the items came in `response.output_item.done`).
    #[serde(default)]
    pub output: Vec<Item>,
    /// Tokens used (`null` until it completes).
    #[serde(default)]
    pub usage: Option<ResponseUsage>,
    /// Why it failed (`null` unless it did).
    #[serde(default)]
    pub error: Option<ResponseError>,
    /// Why it is incomplete (`null` unless it is).
    #[serde(default)]
    pub incomplete_details: Option<IncompleteDetails>,
}

/// The constant `object` of a [`Response`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseTag {
    /// `response`.
    #[default]
    Response,
}

open_str! {
    /// Where a response is.
    pub enum ResponseStatus {
        /// Waiting to start.
        "queued" => Queued,
        /// Being produced.
        "in_progress" => InProgress,
        /// Done.
        "completed" => Completed,
        /// Cut short (see its `incomplete_details`).
        "incomplete" => Incomplete,
        /// Failed (see its `error`).
        "failed" => Failed,
        /// Cancelled.
        "cancelled" => Cancelled,
    }
}

/// Tokens a response used. The API (and Codex) expect every field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseUsage {
    /// Input tokens, cached ones included.
    #[serde(default)]
    pub input_tokens: u64,
    /// The input tokens' breakdown.
    #[serde(default)]
    pub input_tokens_details: InputTokensDetails,
    /// Output tokens, reasoning included.
    #[serde(default)]
    pub output_tokens: u64,
    /// The output tokens' breakdown.
    #[serde(default)]
    pub output_tokens_details: OutputTokensDetails,
    /// Input and output.
    #[serde(default)]
    pub total_tokens: u64,
}

/// The input tokens' breakdown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputTokensDetails {
    /// Input tokens read from the prompt cache.
    #[serde(default)]
    pub cached_tokens: u64,
}

/// The output tokens' breakdown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputTokensDetails {
    /// Output tokens spent reasoning.
    #[serde(default)]
    pub reasoning_tokens: u64,
}

/// Why a response failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseError {
    /// Its class (Codex acts on some).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
    /// What went wrong, for a person.
    pub message: String,
}

open_str! {
    /// The class of a failure. Codex acts on these: it retries the
    /// transient ones and tells the user about the rest.
    pub enum ErrorCode {
        /// The provider failed.
        "server_error" => ServerError,
        /// Too many requests; retried.
        "rate_limit_exceeded" => RateLimitExceeded,
        /// The provider is overloaded; retried.
        "server_is_overloaded" => ServerIsOverloaded,
        /// The conversation outgrew the context window.
        "context_length_exceeded" => ContextLengthExceeded,
        /// The account is out of quota.
        "insufficient_quota" => InsufficientQuota,
        /// The prompt was refused.
        "invalid_prompt" => InvalidPrompt,
    }
}

/// Why a response is incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncompleteDetails {
    /// The reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<IncompleteReason>,
}

open_str! {
    /// Why a response is incomplete.
    pub enum IncompleteReason {
        /// It reached `max_output_tokens`.
        "max_output_tokens" => MaxOutputTokens,
        /// A content filter stopped it.
        "content_filter" => ContentFilter,
    }
}

open_enum! {
    /// One event of a streamed response.
    pub enum StreamEvent in "type" {
        /// The response exists.
        "response.created" => Created(ResponseEvent),
        /// The response is being produced.
        "response.in_progress" => InProgress(ResponseEvent),
        /// The response is done: a proper end of the stream.
        "response.completed" => Completed(ResponseEvent),
        /// The response is cut short: a proper end of the stream.
        "response.incomplete" => Incomplete(ResponseEvent),
        /// The response failed: the end of the stream.
        "response.failed" => Failed(ResponseEvent),
        /// An output item starts.
        "response.output_item.added" => OutputItemAdded(OutputItemEvent),
        /// An output item is done, whole.
        "response.output_item.done" => OutputItemDone(OutputItemEvent),
        /// A message's content part starts.
        "response.content_part.added" => ContentPartAdded(ContentPartEvent),
        /// A message's content part is done.
        "response.content_part.done" => ContentPartDone(ContentPartEvent),
        /// Text grows.
        "response.output_text.delta" => OutputTextDelta(TextDeltaEvent),
        /// Text is done.
        "response.output_text.done" => OutputTextDone(TextDoneEvent),
        /// A refusal grows.
        "response.refusal.delta" => RefusalDelta(TextDeltaEvent),
        /// A refusal is done.
        "response.refusal.done" => RefusalDone(RefusalDoneEvent),
        /// A function call's arguments grow.
        "response.function_call_arguments.delta" => FunctionCallArgumentsDelta(CallDeltaEvent),
        /// A function call's arguments are done.
        "response.function_call_arguments.done" => FunctionCallArgumentsDone(ArgumentsDoneEvent),
        /// A free-form call's input grows.
        "response.custom_tool_call_input.delta" => CustomToolCallInputDelta(CallDeltaEvent),
        /// A free-form call's input is done.
        "response.custom_tool_call_input.done" => CustomToolCallInputDone(InputDoneEvent),
        /// A reasoning summary's part starts.
        "response.reasoning_summary_part.added" => ReasoningSummaryPartAdded(SummaryPartEvent),
        /// A reasoning summary's part is done.
        "response.reasoning_summary_part.done" => ReasoningSummaryPartDone(SummaryPartEvent),
        /// A reasoning summary grows.
        "response.reasoning_summary_text.delta" => ReasoningSummaryTextDelta(SummaryDeltaEvent),
        /// A reasoning summary is done.
        "response.reasoning_summary_text.done" => ReasoningSummaryTextDone(SummaryDoneEvent),
        /// Reasoning shown in full grows.
        "response.reasoning_text.delta" => ReasoningTextDelta(TextDeltaEvent),
        /// Reasoning shown in full is done.
        "response.reasoning_text.done" => ReasoningTextDone(TextDoneEvent),
        /// The stream failed: its end.
        "error" => Error(ErrorEvent),
    }
}

/// An event carrying the whole response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseEvent {
    /// The response so far.
    pub response: Response,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// An output item's start or end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputItemEvent {
    /// The item's place in the output.
    pub output_index: u32,
    /// The item.
    pub item: Item,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// A content part's start or end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentPartEvent {
    /// The message.
    pub item_id: String,
    /// The message's place in the output.
    pub output_index: u32,
    /// The part's place in the message.
    pub content_index: u32,
    /// The part.
    pub part: ContentPart,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// Text that grows, in a message part or in reasoning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDeltaEvent {
    /// The item.
    pub item_id: String,
    /// The item's place in the output.
    pub output_index: u32,
    /// The part's place in the item.
    pub content_index: u32,
    /// What it adds.
    pub delta: String,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// Text that is done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDoneEvent {
    /// The item.
    pub item_id: String,
    /// The item's place in the output.
    pub output_index: u32,
    /// The part's place in the item.
    pub content_index: u32,
    /// The whole text.
    pub text: String,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// A refusal that is done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalDoneEvent {
    /// The message.
    pub item_id: String,
    /// The message's place in the output.
    pub output_index: u32,
    /// The part's place in the message.
    pub content_index: u32,
    /// The whole refusal.
    pub refusal: String,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// A tool call's arguments or input that grows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallDeltaEvent {
    /// The call's item.
    pub item_id: String,
    /// The item's place in the output.
    pub output_index: u32,
    /// What it adds.
    pub delta: String,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// A function call's arguments, done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgumentsDoneEvent {
    /// The call's item.
    pub item_id: String,
    /// The item's place in the output.
    pub output_index: u32,
    /// The whole arguments.
    pub arguments: String,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// A free-form call's input, done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDoneEvent {
    /// The call's item.
    pub item_id: String,
    /// The item's place in the output.
    pub output_index: u32,
    /// The whole input.
    pub input: String,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// A reasoning summary's part, starting or done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryPartEvent {
    /// The reasoning item.
    pub item_id: String,
    /// The item's place in the output.
    pub output_index: u32,
    /// The part's place in the summary.
    pub summary_index: u32,
    /// The part.
    pub part: SummaryPart,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// A reasoning summary that grows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryDeltaEvent {
    /// The reasoning item.
    pub item_id: String,
    /// The item's place in the output.
    pub output_index: u32,
    /// The part's place in the summary.
    pub summary_index: u32,
    /// What it adds.
    pub delta: String,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// A reasoning summary that is done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryDoneEvent {
    /// The reasoning item.
    pub item_id: String,
    /// The item's place in the output.
    pub output_index: u32,
    /// The part's place in the summary.
    pub summary_index: u32,
    /// The whole part.
    pub text: String,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// An `error` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEvent {
    /// Its class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
    /// What went wrong, for a person.
    pub message: String,
    /// The event's place in the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<u64>,
}

/// The body of an error response (any status but 200), shared by the
/// Responses and Chat Completions APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// The error.
    pub error: ErrorDetail,
}

/// An error in an error body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// What went wrong, for a person.
    pub message: String,
    /// Its kind (`invalid_request_error`, `server_error`, ...).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ErrorType>,
    /// Its class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
}

open_str! {
    /// The kind of an error body's error.
    pub enum ErrorType {
        /// The request is wrong.
        "invalid_request_error" => InvalidRequest,
        /// The key is wrong.
        "authentication_error" => Authentication,
        /// Too many requests.
        "rate_limit_error" => RateLimit,
        /// The provider failed.
        "server_error" => Server,
        /// The account is out of quota.
        "insufficient_quota" => InsufficientQuota,
    }
}

#[cfg(test)]
mod tests;
