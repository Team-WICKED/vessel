//! An `OpenAI` Responses upstream: the neutral request out as a Responses
//! request, its stream in as neutral events.
//!
//! Vessel keeps nothing at the provider: every request is stateless
//! (`store: false`) and asks for its reasoning encrypted
//! (`include: ["reasoning.encrypted_content"]`), so the reasoning travels
//! with the harness's history (as a Vessel envelope where the harness
//! speaks another API) and comes back on the next turn. `OpenAI` refuses a
//! tool loop whose reasoning is missing, so this is not optional.

use std::collections::BTreeSet;

use crate::error::TranslateError;
use crate::ir;
use crate::sse::SseEvent;
use crate::wire::responses::{
    self as wire, CallOutputItem, ContentPart, CustomFormat, CustomTool, CustomToolCallItem,
    ErrorCode, FunctionCallItem, FunctionTool, Grammar, GrammarSyntax, Include, IncompleteReason,
    InputFile, InputImage, Item, MessageItem, NamedToolChoice, OutputText, Reasoning,
    ReasoningEffort, ReasoningItem, ReasoningSummary, ResponseError, ResponseUsage, Role,
    StreamEvent, SummaryPart, ToolChoiceMode, ToolName,
};
use crate::wire::{Nothing, StrOr, TextOr, TextPart};

/// What a Responses provider takes, from its configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The node's name for the provider: only reasoning it made goes back
    /// to it.
    pub provider: String,
    /// The provider takes sampling settings (`OpenAI`'s reasoning models
    /// refuse a temperature).
    pub sampling: bool,
}

/// The neutral request as a Responses request, streamed and stateless.
///
/// # Errors
/// A free-form tool's schema could not be written (it can).
pub fn request(request: &ir::Request, options: &Options) -> Result<wire::Request, TranslateError> {
    // A free-form call's result is a free-form output; the API checks.
    let freeform_calls: BTreeSet<&str> = request
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            ir::Part::ToolCall(ir::ToolCall {
                id,
                input: ir::ToolInput::Text(_),
                ..
            }) => Some(id.as_str()),
            ir::Part::ToolCall(_)
            | ir::Part::Text(_)
            | ir::Part::Image(_)
            | ir::Part::Document(_)
            | ir::Part::Thinking(_)
            | ir::Part::ToolResult(_) => None,
        })
        .collect();
    let mut input = Vec::new();
    for message in &request.messages {
        match message.role {
            ir::Role::User => user(message, &freeform_calls, &mut input),
            ir::Role::Assistant => assistant(message, options, &mut input),
        }
    }

    let tools: Vec<wire::Tool> = request.tools.iter().map(tool).collect();
    let tool_choice = (!tools.is_empty()).then(|| match &request.tool_choice {
        ir::ToolChoice::Auto => StrOr::Str(ToolChoiceMode::Auto),
        ir::ToolChoice::Any => StrOr::Str(ToolChoiceMode::Required),
        ir::ToolChoice::None => StrOr::Str(ToolChoiceMode::None),
        ir::ToolChoice::Tool(name) => {
            let named = ToolName { name: name.clone() };
            let freeform = request.tools.iter().any(|tool| {
                tool.name == *name && matches!(tool.kind, ir::ToolKind::Freeform { .. })
            });
            StrOr::Object(if freeform {
                NamedToolChoice::Custom(named)
            } else {
                NamedToolChoice::Function(named)
            })
        }
    });

    Ok(wire::Request {
        model: request.model.clone(),
        instructions: (!request.system.is_empty()).then(|| request.system.join("\n\n")),
        input: Some(TextOr::Parts(input)),
        tools,
        tool_choice,
        parallel_tool_calls: request.parallel_tool_calls,
        reasoning: request.reasoning.effort().map(|effort| Reasoning {
            effort: Some(effort_of(effort)),
            summary: Some(ReasoningSummary::Auto),
        }),
        store: Some(false),
        stream: Some(true),
        include: vec![Include::ReasoningEncryptedContent],
        prompt_cache_key: request.cache_key.clone(),
        text: None,
        max_output_tokens: request.max_output_tokens,
        temperature: request.sampling.temperature.filter(|_| options.sampling),
        top_p: request.sampling.top_p.filter(|_| options.sampling),
        previous_response_id: None,
    })
}

const fn effort_of(effort: ir::Effort) -> ReasoningEffort {
    match effort {
        ir::Effort::Minimal => ReasoningEffort::Minimal,
        ir::Effort::Low => ReasoningEffort::Low,
        ir::Effort::Medium => ReasoningEffort::Medium,
        ir::Effort::High => ReasoningEffort::High,
        ir::Effort::Max => ReasoningEffort::ExtraHigh,
    }
}

fn tool(tool: &ir::Tool) -> wire::Tool {
    match &tool.kind {
        ir::ToolKind::Function { parameters, strict } => wire::Tool::Function(FunctionTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: parameters.clone(),
            strict: *strict,
        }),
        ir::ToolKind::Freeform { grammar } => wire::Tool::Custom(CustomTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            format: Some(grammar.as_ref().map_or(
                CustomFormat::Text(Nothing {}),
                |(syntax, definition)| {
                    CustomFormat::Grammar(Grammar {
                        syntax: GrammarSyntax::parse(syntax),
                        definition: definition.clone(),
                    })
                },
            )),
        }),
    }
}

fn user(message: &ir::Message, freeform_calls: &BTreeSet<&str>, input: &mut Vec<Item>) {
    let mut content = Vec::new();
    for part in &message.parts {
        match part {
            ir::Part::ToolResult(result) => {
                let output = CallOutputItem {
                    id: None,
                    call_id: result.call_id.clone(),
                    output: result_output(result),
                };
                input.push(if freeform_calls.contains(result.call_id.as_str()) {
                    Item::CustomToolCallOutput(output)
                } else {
                    Item::FunctionCallOutput(output)
                });
            }
            ir::Part::Text(text) => {
                content.push(ContentPart::InputText(TextPart { text: text.clone() }));
            }
            ir::Part::Image(image) => content.push(image_part(image)),
            ir::Part::Document(ir::Document::Pdf { data, title }) => {
                content.push(ContentPart::InputFile(InputFile {
                    file_id: None,
                    file_data: Some(format!("data:application/pdf;base64,{data}")),
                    filename: Some(title.clone().unwrap_or_else(|| "document.pdf".to_owned())),
                    file_url: None,
                }));
            }
            ir::Part::Document(ir::Document::Text { text, title }) => {
                let text = match title {
                    Some(title) => format!("[Document: {title}]\n{text}"),
                    None => text.clone(),
                };
                content.push(ContentPart::InputText(TextPart { text }));
            }
            // Not in a user turn.
            ir::Part::Thinking(_) | ir::Part::ToolCall(_) => {}
        }
    }
    if !content.is_empty() {
        input.push(Item::Message(MessageItem {
            id: None,
            role: Role::User,
            content: TextOr::Parts(content),
            status: None,
        }));
    }
}

fn image_part(image: &ir::Image) -> ContentPart {
    ContentPart::InputImage(InputImage {
        image_url: Some(image.to_url().into_owned()),
        file_id: None,
        detail: None,
    })
}

fn result_output(result: &ir::ToolResult) -> TextOr<ContentPart> {
    let mut text = result.text();
    if result.is_error {
        text.insert_str(0, "[The tool failed.]\n");
    }
    let has_images = result
        .content
        .iter()
        .any(|part| matches!(part, ir::ResultPart::Image(_)));
    if !has_images {
        return TextOr::Text(text);
    }
    let mut parts = Vec::with_capacity(result.content.len().saturating_add(1));
    if result.is_error {
        parts.push(ContentPart::InputText(TextPart {
            text: "[The tool failed.]".to_owned(),
        }));
    }
    for part in &result.content {
        parts.push(match part {
            ir::ResultPart::Text(text) => ContentPart::InputText(TextPart { text: text.clone() }),
            ir::ResultPart::Image(image) => image_part(image),
        });
    }
    TextOr::Parts(parts)
}

fn assistant(message: &ir::Message, options: &Options, input: &mut Vec<Item>) {
    for part in &message.parts {
        match part {
            ir::Part::Text(text) => input.push(Item::Message(MessageItem {
                id: None,
                role: Role::Assistant,
                content: TextOr::Parts(vec![ContentPart::OutputText(OutputText {
                    text: text.clone(),
                    annotations: Vec::new(),
                })]),
                status: None,
            })),
            ir::Part::Thinking(thinking) => {
                let ours = thinking
                    .source
                    .as_deref()
                    .is_none_or(|source| source == options.provider);
                if let (true, ir::Replay::ResponsesEncrypted(content)) = (ours, &thinking.replay) {
                    input.push(Item::Reasoning(ReasoningItem {
                        id: None,
                        summary: if thinking.text.is_empty() {
                            Vec::new()
                        } else {
                            vec![SummaryPart::Text(TextPart {
                                text: thinking.text.clone(),
                            })]
                        },
                        content: None,
                        encrypted_content: Some(content.clone()),
                        status: None,
                    }));
                }
            }
            ir::Part::ToolCall(call) => input.push(match &call.input {
                ir::ToolInput::Json(arguments) => Item::FunctionCall(FunctionCallItem {
                    id: None,
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: if arguments.trim().is_empty() {
                        String::from("{}")
                    } else {
                        arguments.clone()
                    },
                    status: None,
                }),
                ir::ToolInput::Text(text) => Item::CustomToolCall(CustomToolCallItem {
                    id: None,
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    input: text.clone(),
                    status: None,
                }),
            }),
            // Not in an assistant turn.
            ir::Part::Image(_) | ir::Part::Document(_) | ir::Part::ToolResult(_) => {}
        }
    }
}

/// The item open in the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Open {
    None,
    Reasoning { summaries: u32 },
    Call { args_streamed: bool },
    Custom { input: String },
}

/// Reads a Responses stream into neutral events.
#[derive(Debug)]
pub struct StreamDecoder {
    open: Open,
    calls: bool,
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
            calls: false,
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
            StreamEvent::OutputItemAdded(added) => self.added(added.item, out),
            StreamEvent::OutputItemDone(done) => self.done(done.item, out),
            StreamEvent::ReasoningSummaryPartAdded(_) => {
                if let Open::Reasoning { summaries } = &mut self.open {
                    if *summaries > 0 {
                        out.push(ir::Event::Thinking(String::from("\n\n")));
                    }
                    *summaries = summaries.saturating_add(1);
                }
            }
            StreamEvent::ReasoningSummaryTextDelta(delta) => {
                out.push(ir::Event::Thinking(delta.delta));
            }
            StreamEvent::ReasoningTextDelta(delta) => out.push(ir::Event::Thinking(delta.delta)),
            StreamEvent::OutputTextDelta(delta) | StreamEvent::RefusalDelta(delta) => {
                out.push(ir::Event::Text(delta.delta));
            }
            StreamEvent::FunctionCallArgumentsDelta(delta) => {
                if let Open::Call { args_streamed } = &mut self.open {
                    *args_streamed = true;
                    out.push(ir::Event::ToolArgs(delta.delta));
                }
            }
            StreamEvent::CustomToolCallInputDelta(delta) => {
                if let Open::Custom { input } = &mut self.open {
                    input.push_str(&delta.delta);
                }
            }
            StreamEvent::Completed(end) | StreamEvent::Incomplete(end) => {
                let response = end.response;
                if let Some(usage) = response.usage {
                    out.push(ir::Event::Usage(usage_of(&usage)));
                }
                let reason = response
                    .incomplete_details
                    .and_then(|details| details.reason);
                out.push(ir::Event::Stop(match reason {
                    Some(IncompleteReason::MaxOutputTokens) => ir::StopReason::MaxTokens,
                    Some(IncompleteReason::ContentFilter) => ir::StopReason::Refusal,
                    Some(IncompleteReason::Unknown(_)) | None if self.calls => {
                        ir::StopReason::ToolUse
                    }
                    Some(IncompleteReason::Unknown(_)) | None => ir::StopReason::EndTurn,
                }));
                self.ended = true;
            }
            StreamEvent::Failed(end) => {
                let failure = end.response.error.as_ref().map_or_else(
                    || ir::Failure::new(ir::FailureClass::UpstreamError, "the response failed"),
                    failure,
                );
                self.fail(failure, out);
            }
            StreamEvent::Error(error) => self.fail(
                failure(&ResponseError {
                    code: error.code,
                    message: error.message,
                }),
                out,
            ),
            StreamEvent::Created(_)
            | StreamEvent::InProgress(_)
            | StreamEvent::ContentPartAdded(_)
            | StreamEvent::ContentPartDone(_)
            | StreamEvent::OutputTextDone(_)
            | StreamEvent::RefusalDone(_)
            | StreamEvent::FunctionCallArgumentsDone(_)
            | StreamEvent::CustomToolCallInputDone(_)
            | StreamEvent::ReasoningSummaryPartDone(_)
            | StreamEvent::ReasoningSummaryTextDone(_)
            | StreamEvent::ReasoningTextDone(_)
            | StreamEvent::Unknown { .. } => {}
        }
    }

    fn added(&mut self, item: Item, out: &mut Vec<ir::Event>) {
        match item {
            Item::Reasoning(_) => self.open = Open::Reasoning { summaries: 0 },
            Item::FunctionCall(call) => {
                self.calls = true;
                out.push(ir::Event::ToolCallStart {
                    id: call.call_id,
                    name: call.name,
                });
                self.open = Open::Call {
                    args_streamed: false,
                };
            }
            Item::CustomToolCall(call) => {
                self.calls = true;
                out.push(ir::Event::ToolCallStart {
                    id: call.call_id,
                    name: call.name,
                });
                self.open = Open::Custom {
                    input: String::new(),
                };
            }
            Item::Message(_)
            | Item::FunctionCallOutput(_)
            | Item::CustomToolCallOutput(_)
            | Item::Unknown { .. } => self.open = Open::None,
        }
    }

    fn done(&mut self, item: Item, out: &mut Vec<ir::Event>) {
        let open = std::mem::replace(&mut self.open, Open::None);
        match (open, item) {
            (Open::Reasoning { .. }, Item::Reasoning(reasoning)) => {
                out.push(ir::Event::ThinkingEnd(
                    reasoning
                        .encrypted_content
                        .map_or(ir::Replay::Text, ir::Replay::ResponsesEncrypted),
                ));
            }
            (Open::Call { args_streamed }, Item::FunctionCall(call)) => {
                // Some servers send the arguments only whole, at the end.
                if !args_streamed {
                    out.push(ir::Event::ToolArgs(call.arguments));
                }
                out.push(ir::Event::ToolCallEnd);
            }
            (Open::Custom { input }, Item::CustomToolCall(call)) => {
                let text = if call.input.is_empty() {
                    input
                } else {
                    call.input
                };
                out.push(ir::Event::ToolArgs(ir::freeform_arguments(&text)));
                out.push(ir::Event::ToolCallEnd);
            }
            _unmatched => {}
        }
    }

    fn fail(&mut self, failure: ir::Failure, out: &mut Vec<ir::Event>) {
        self.ended = true;
        out.push(ir::Event::Failed(failure));
    }

    /// The stream ended: a truncation unless the response ended first.
    pub fn finish(&mut self, out: &mut Vec<ir::Event>) {
        if !self.ended {
            self.fail(
                ir::Failure::new(
                    ir::FailureClass::Truncated,
                    "the provider's stream ended before the response did",
                ),
                out,
            );
        }
    }
}

fn usage_of(usage: &ResponseUsage) -> ir::Usage {
    let cached = usage.input_tokens_details.cached_tokens;
    ir::Usage {
        input: usage.input_tokens.saturating_sub(cached),
        cache_read: cached,
        cache_write: 0,
        output: usage.output_tokens,
        reasoning: usage.output_tokens_details.reasoning_tokens,
    }
}

/// A Responses error as a neutral failure.
#[must_use]
pub fn failure(error: &ResponseError) -> ir::Failure {
    let class = match &error.code {
        Some(ErrorCode::RateLimitExceeded) => ir::FailureClass::RateLimited,
        Some(ErrorCode::ServerIsOverloaded) => ir::FailureClass::Overloaded,
        Some(ErrorCode::ContextLengthExceeded) => ir::FailureClass::ContextTooLong,
        Some(ErrorCode::InsufficientQuota) => ir::FailureClass::Unauthorized,
        Some(ErrorCode::InvalidPrompt) => ir::FailureClass::RefusedRequest,
        Some(ErrorCode::ServerError | ErrorCode::Unknown(_)) | None => {
            ir::FailureClass::UpstreamError
        }
    };
    ir::Failure::new(class, error.message.clone())
}

#[cfg(test)]
mod tests;
