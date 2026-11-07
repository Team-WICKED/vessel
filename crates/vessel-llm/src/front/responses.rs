//! The `OpenAI` Responses front, which Codex speaks: its request into the
//! neutral form, the neutral stream out as Responses events, and failures
//! as Codex expects them.
//!
//! What Codex needs, learned from opencodex and Codex's own stream reader:
//! - every output item announced (`response.output_item.added`) and then
//!   given whole (`response.output_item.done`): Codex keeps the done items
//!   as its history;
//! - free-form tools (`apply_patch`) as `custom_tool_call` items with their
//!   raw text, though a provider without free-form tools called them as a
//!   function of one string ([`ir::freeform_text`] unwraps it);
//! - reasoning items with `encrypted_content`, which Codex sends back on
//!   the next turn: for another API's reasoning, a Vessel envelope;
//! - failures inside the stream (`response.failed`) with the codes Codex
//!   acts on: it retries the transient ones, waiting as long as the message
//!   says ("Please try again in 5s"), and compacts on
//!   `context_length_exceeded`;
//! - exactly one of `response.completed`, `response.incomplete` and
//!   `response.failed` at the end.

use std::collections::BTreeSet;

use crate::error::{Dropped, TranslateError};
use crate::ir::{self, envelope};
use crate::wire::responses::{
    self as wire, CallOutputItem, ContentPart, ContentPartEvent, CustomToolCallItem, ErrorBody,
    ErrorCode, ErrorDetail, ErrorType, FunctionCallItem, IncompleteDetails, IncompleteReason,
    InputTokensDetails, Item, ItemStatus, MessageItem, NamedToolChoice, OutputItemEvent,
    OutputText, OutputTokensDetails, ReasoningEffort, ReasoningItem, Response, ResponseError,
    ResponseEvent, ResponseStatus, ResponseTag, ResponseUsage, Role, StreamEvent, SummaryPart,
    SummaryPartEvent, TextDeltaEvent, TextDoneEvent, ToolChoiceMode,
};
use crate::wire::{StrOr, TextOr, TextPart};

/// Codex's request in the neutral form, and what it held that the neutral
/// form cannot say (to log).
///
/// # Errors
/// The request continues a response kept at a provider
/// (`previous_response_id`), or holds an image or a file by id.
pub fn request(request: wire::Request) -> Result<(ir::Request, Vec<Dropped>), TranslateError> {
    if request.previous_response_id.is_some() {
        return Err(TranslateError::Unsupported(
            "a response kept at another provider (`previous_response_id`)",
        ));
    }
    let mut dropped = Vec::new();
    let mut system: Vec<String> = request.instructions.into_iter().collect();
    let mut messages: Vec<ir::Message> = Vec::new();
    let items = match request.input {
        None => Vec::new(),
        Some(TextOr::Text(text)) => vec![Item::Message(MessageItem {
            id: None,
            role: Role::User,
            content: TextOr::Text(text),
            status: None,
        })],
        Some(TextOr::Parts(items)) => items,
    };
    for item in items {
        let (role, part) = match item {
            Item::Message(message) => {
                match message.role {
                    Role::System | Role::Developer => {
                        system.push(text_of(message.content, &mut dropped));
                    }
                    Role::User => {
                        for part in content(message.content, &mut dropped)? {
                            push(&mut messages, ir::Role::User, part);
                        }
                    }
                    Role::Assistant => {
                        let text = text_of(message.content, &mut dropped);
                        push(&mut messages, ir::Role::Assistant, ir::Part::Text(text));
                    }
                }
                continue;
            }
            Item::Reasoning(reasoning) => {
                (ir::Role::Assistant, ir::Part::Thinking(thinking(reasoning)))
            }
            Item::FunctionCall(call) => (
                ir::Role::Assistant,
                ir::Part::ToolCall(ir::ToolCall {
                    id: call.call_id,
                    name: call.name,
                    input: ir::ToolInput::Json(call.arguments),
                }),
            ),
            Item::CustomToolCall(call) => (
                ir::Role::Assistant,
                ir::Part::ToolCall(ir::ToolCall {
                    id: call.call_id,
                    name: call.name,
                    input: ir::ToolInput::Text(call.input),
                }),
            ),
            Item::FunctionCallOutput(output) | Item::CustomToolCallOutput(output) => (
                ir::Role::User,
                ir::Part::ToolResult(result(output, &mut dropped)?),
            ),
            Item::Unknown { kind, .. } => {
                dropped.push(Dropped::new(kind, "item"));
                continue;
            }
        };
        push(&mut messages, role, part);
    }
    ir::repair(&mut messages);

    Ok((
        ir::Request {
            model: request.model,
            system,
            messages,
            tools: tools(request.tools, &mut dropped),
            tool_choice: tool_choice(request.tool_choice),
            parallel_tool_calls: request.parallel_tool_calls,
            max_output_tokens: request.max_output_tokens,
            sampling: ir::Sampling {
                temperature: request.temperature,
                top_p: request.top_p,
                top_k: None,
            },
            stop_sequences: Vec::new(),
            reasoning: request
                .reasoning
                .and_then(|reasoning| reasoning.effort)
                .map_or(ir::Reasoning::Default, |effort| reasoning_of(&effort)),
            cache_key: request.prompt_cache_key.map(|key| ir::cache_key(&key)),
        },
        dropped,
    ))
}

fn push(messages: &mut Vec<ir::Message>, role: ir::Role, part: ir::Part) {
    match messages.last_mut() {
        Some(last) if last.role == role => last.parts.push(part),
        Some(_) | None => messages.push(ir::Message {
            role,
            parts: vec![part],
        }),
    }
}

/// A reasoning item as Codex sends it back: `OpenAI`'s own encrypted
/// content, or one of Vessel's envelopes.
fn thinking(reasoning: ReasoningItem) -> ir::Thinking {
    let summary: Vec<String> = reasoning
        .summary
        .into_iter()
        .filter_map(|part| match part {
            SummaryPart::Text(text) => Some(text.text),
            SummaryPart::Unknown { .. } => None,
        })
        .collect();
    let text = summary.join("\n\n");
    match reasoning.encrypted_content {
        None => ir::Thinking {
            text,
            replay: ir::Replay::Text,
            source: None,
        },
        Some(content) => match envelope::open(&content) {
            Some((source, replay)) => ir::Thinking {
                text,
                replay,
                source: Some(source),
            },
            None => ir::Thinking {
                text,
                replay: ir::Replay::ResponsesEncrypted(content),
                source: None,
            },
        },
    }
}

/// A message's content as text, for the system prompt or the model's own
/// words.
fn text_of(content: TextOr<ContentPart>, dropped: &mut Vec<Dropped>) -> String {
    match content {
        TextOr::Text(text) => text,
        TextOr::Parts(parts) => {
            let mut text = String::new();
            for part in parts {
                match part {
                    ContentPart::InputText(part) => text.push_str(&part.text),
                    ContentPart::OutputText(part) => text.push_str(&part.text),
                    ContentPart::Refusal(part) => text.push_str(&part.refusal),
                    ContentPart::InputImage(_) | ContentPart::InputFile(_) => {
                        dropped.push(Dropped::new(part_kind(&part), "message"));
                    }
                    ContentPart::Unknown { kind, .. } => {
                        dropped.push(Dropped::new(kind, "message"));
                    }
                }
            }
            text
        }
    }
}

fn part_kind(part: &ContentPart) -> String {
    part.kind().to_owned()
}

/// A user message's content.
fn content(
    content: TextOr<ContentPart>,
    dropped: &mut Vec<Dropped>,
) -> Result<Vec<ir::Part>, TranslateError> {
    let parts = match content {
        TextOr::Text(text) => return Ok(vec![ir::Part::Text(text)]),
        TextOr::Parts(parts) => parts,
    };
    let mut neutral = Vec::with_capacity(parts.len());
    for part in parts {
        neutral.push(match part {
            ContentPart::InputText(part) => ir::Part::Text(part.text),
            ContentPart::OutputText(part) => ir::Part::Text(part.text),
            ContentPart::Refusal(part) => ir::Part::Text(part.refusal),
            ContentPart::InputImage(image) => ir::Part::Image(self::image(image)?),
            ContentPart::InputFile(file) => {
                let Some(data) = file
                    .file_data
                    .as_deref()
                    .and_then(|data| data.strip_prefix("data:application/pdf;base64,"))
                else {
                    return Err(TranslateError::Unsupported(
                        "a file other than an inline PDF (files live at one provider)",
                    ));
                };
                ir::Part::Document(ir::Document::Pdf {
                    data: data.to_owned(),
                    title: file.filename,
                })
            }
            ContentPart::Unknown { kind, .. } => {
                dropped.push(Dropped::new(kind, "message"));
                continue;
            }
        });
    }
    Ok(neutral)
}

fn image(image: wire::InputImage) -> Result<ir::Image, TranslateError> {
    image.image_url.map_or(
        Err(TranslateError::Unsupported(
            "an image by file id (files live at one provider)",
        )),
        |url| Ok(ir::Image::from_url(&url)),
    )
}

fn result(
    output: CallOutputItem,
    dropped: &mut Vec<Dropped>,
) -> Result<ir::ToolResult, TranslateError> {
    let content = match output.output {
        TextOr::Text(text) => vec![ir::ResultPart::Text(text)],
        TextOr::Parts(parts) => {
            let mut content = Vec::with_capacity(parts.len());
            for part in content_parts(parts, dropped)? {
                match part {
                    ir::Part::Image(image) => content.push(ir::ResultPart::Image(image)),
                    ir::Part::Text(text) => content.push(ir::ResultPart::Text(text)),
                    ir::Part::Document(_)
                    | ir::Part::Thinking(_)
                    | ir::Part::ToolCall(_)
                    | ir::Part::ToolResult(_) => {
                        dropped.push(Dropped::new("input_file", "tool result"));
                    }
                }
            }
            content
        }
    };
    Ok(ir::ToolResult {
        call_id: output.call_id,
        content,
        is_error: false,
    })
}

fn content_parts(
    parts: Vec<ContentPart>,
    dropped: &mut Vec<Dropped>,
) -> Result<Vec<ir::Part>, TranslateError> {
    content(TextOr::Parts(parts), dropped)
}

fn tools(tools: Vec<wire::Tool>, dropped: &mut Vec<Dropped>) -> Vec<ir::Tool> {
    let mut neutral = Vec::with_capacity(tools.len());
    for tool in tools {
        match tool {
            wire::Tool::Function(tool) => neutral.push(ir::Tool {
                name: tool.name,
                description: tool.description,
                kind: ir::ToolKind::Function {
                    parameters: tool.parameters,
                    strict: tool.strict,
                },
            }),
            wire::Tool::Custom(tool) => {
                let grammar = match tool.format {
                    Some(wire::CustomFormat::Grammar(grammar)) => {
                        Some((grammar.syntax.as_str().to_owned(), grammar.definition))
                    }
                    Some(wire::CustomFormat::Text(_) | wire::CustomFormat::Unknown { .. })
                    | None => None,
                };
                neutral.push(ir::Tool {
                    name: tool.name,
                    description: tool.description,
                    kind: ir::ToolKind::Freeform { grammar },
                });
            }
            // Hosted tools run at `OpenAI`; elsewhere there is nothing to
            // run them.
            wire::Tool::Unknown { kind, .. } => dropped.push(Dropped::new(kind, "tool")),
        }
    }
    neutral
}

fn tool_choice(choice: Option<wire::ToolChoice>) -> ir::ToolChoice {
    match choice {
        None
        | Some(
            StrOr::Str(ToolChoiceMode::Auto | ToolChoiceMode::Unknown(_))
            | StrOr::Object(NamedToolChoice::Unknown { .. }),
        ) => ir::ToolChoice::Auto,
        Some(StrOr::Str(ToolChoiceMode::None)) => ir::ToolChoice::None,
        Some(StrOr::Str(ToolChoiceMode::Required)) => ir::ToolChoice::Any,
        Some(StrOr::Object(NamedToolChoice::Function(named) | NamedToolChoice::Custom(named))) => {
            ir::ToolChoice::Tool(named.name)
        }
    }
}

fn reasoning_of(effort: &ReasoningEffort) -> ir::Reasoning {
    match effort {
        ReasoningEffort::None => ir::Reasoning::Off,
        ReasoningEffort::Minimal => ir::Reasoning::Effort(ir::Effort::Minimal),
        ReasoningEffort::Low => ir::Reasoning::Effort(ir::Effort::Low),
        ReasoningEffort::Medium => ir::Reasoning::Effort(ir::Effort::Medium),
        ReasoningEffort::High => ir::Reasoning::Effort(ir::Effort::High),
        ReasoningEffort::ExtraHigh => ir::Reasoning::Effort(ir::Effort::Max),
        ReasoningEffort::Unknown(_) => ir::Reasoning::Default,
    }
}

/// The item open in the stream.
#[derive(Debug)]
enum Open {
    Message {
        id: String,
        text: String,
    },
    Reasoning {
        id: String,
        /// The summary so far; `None` until its part is announced.
        summary: Option<String>,
    },
    Call {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        /// A free-form tool, called through its function form.
        freeform: bool,
    },
}

/// Writes the neutral stream as Responses events, for one answer.
#[derive(Debug)]
pub struct StreamEncoder {
    id: String,
    model: String,
    /// The node's name for the provider, sealed into envelopes.
    source: String,
    /// The request's free-form tools, whose calls are unwrapped.
    freeform: BTreeSet<String>,
    started: bool,
    sequence: u64,
    open: Option<Open>,
    output: Vec<Item>,
    usage: ir::Usage,
    stop: Option<ir::StopReason>,
    ended: bool,
}

impl StreamEncoder {
    /// An encoder for an answer from `model`, made by the provider the
    /// node calls `source`, to a request offering `tools`.
    #[must_use]
    pub fn new(model: &str, source: &str, tools: &[ir::Tool]) -> Self {
        Self {
            id: format!("resp_vsl_{}", uuid::Uuid::new_v4().simple()),
            model: model.to_owned(),
            source: source.to_owned(),
            freeform: tools
                .iter()
                .filter(|tool| matches!(tool.kind, ir::ToolKind::Freeform { .. }))
                .map(|tool| tool.name.clone())
                .collect(),
            started: false,
            sequence: 0,
            open: None,
            output: Vec::new(),
            usage: ir::Usage::default(),
            stop: None,
            ended: false,
        }
    }

    /// Whether the stream has ended.
    #[must_use]
    pub const fn ended(&self) -> bool {
        self.ended
    }

    fn next_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        sequence
    }

    fn output_index(&self) -> u32 {
        u32::try_from(self.output.len()).unwrap_or(u32::MAX)
    }

    fn response(&self, status: ResponseStatus) -> Response {
        Response {
            id: self.id.clone(),
            object: ResponseTag::Response,
            status,
            model: self.model.clone(),
            output: self.output.clone(),
            usage: None,
            error: None,
            incomplete_details: None,
        }
    }

    /// Start the response (`response.created`, `response.in_progress`).
    pub fn start(&mut self, out: &mut Vec<StreamEvent>) {
        if self.started || self.ended {
            return;
        }
        self.started = true;
        let response = self.response(ResponseStatus::InProgress);
        let sequence_number = Some(self.next_sequence());
        out.push(StreamEvent::Created(ResponseEvent {
            response: response.clone(),
            sequence_number,
        }));
        let sequence_number = Some(self.next_sequence());
        out.push(StreamEvent::InProgress(ResponseEvent {
            response,
            sequence_number,
        }));
    }

    /// Write one event of the answer.
    pub fn push(&mut self, event: ir::Event, out: &mut Vec<StreamEvent>) {
        if self.ended {
            return;
        }
        self.start(out);
        match event {
            ir::Event::Text(text) => self.text(text, out),
            ir::Event::Thinking(text) => self.thinking(text, out),
            ir::Event::ThinkingEnd(replay) => {
                if !matches!(self.open, Some(Open::Reasoning { .. })) {
                    self.close(out);
                    self.open_reasoning(out);
                }
                let content = envelope::seal(&self.source, &replay);
                self.close_reasoning(Some(content), out);
            }
            ir::Event::ToolCallStart { id, name } => {
                self.close(out);
                let freeform = self.freeform.contains(&name);
                let item_id = format!("fc_vsl_{}", uuid::Uuid::new_v4().simple());
                let item = call_item(&item_id, &id, &name, "", freeform, ItemStatus::InProgress);
                self.added(item, out);
                self.open = Some(Open::Call {
                    id: item_id,
                    call_id: id,
                    name,
                    arguments: String::new(),
                    freeform,
                });
            }
            ir::Event::ToolArgs(piece) => {
                let sequence_number = Some(self.next_sequence());
                let output_index = self.output_index();
                if let Some(Open::Call {
                    id,
                    arguments,
                    freeform,
                    ..
                }) = &mut self.open
                {
                    arguments.push_str(&piece);
                    // A free-form call's text is known only once its
                    // arguments are whole.
                    if !*freeform {
                        out.push(StreamEvent::FunctionCallArgumentsDelta(
                            wire::CallDeltaEvent {
                                item_id: id.clone(),
                                output_index,
                                delta: piece,
                                sequence_number,
                            },
                        ));
                    }
                }
            }
            ir::Event::ToolCallEnd => self.close(out),
            ir::Event::Usage(usage) => self.usage = usage,
            ir::Event::Stop(reason) => self.stop = Some(reason),
            ir::Event::Failed(failure) => self.fail(&failure, out),
        }
    }

    fn text(&mut self, text: String, out: &mut Vec<StreamEvent>) {
        if !matches!(self.open, Some(Open::Message { .. })) {
            self.close(out);
            let id = format!("msg_vsl_{}", uuid::Uuid::new_v4().simple());
            self.added(
                Item::Message(MessageItem {
                    id: Some(id.clone()),
                    role: Role::Assistant,
                    content: TextOr::Parts(Vec::new()),
                    status: Some(ItemStatus::InProgress),
                }),
                out,
            );
            let sequence_number = Some(self.next_sequence());
            out.push(StreamEvent::ContentPartAdded(ContentPartEvent {
                item_id: id.clone(),
                output_index: self.output_index(),
                content_index: 0,
                part: output_text(String::new()),
                sequence_number,
            }));
            self.open = Some(Open::Message {
                id,
                text: String::new(),
            });
        }
        let sequence_number = Some(self.next_sequence());
        let output_index = self.output_index();
        if let Some(Open::Message { id, text: whole }) = &mut self.open {
            whole.push_str(&text);
            out.push(StreamEvent::OutputTextDelta(TextDeltaEvent {
                item_id: id.clone(),
                output_index,
                content_index: 0,
                delta: text,
                sequence_number,
            }));
        }
    }

    fn thinking(&mut self, text: String, out: &mut Vec<StreamEvent>) {
        if !matches!(self.open, Some(Open::Reasoning { .. })) {
            self.close(out);
            self.open_reasoning(out);
        }
        let output_index = self.output_index();
        let announce = matches!(self.open, Some(Open::Reasoning { summary: None, .. }));
        let first = Some(self.next_sequence());
        let second = Some(self.next_sequence());
        if let Some(Open::Reasoning { id, summary }) = &mut self.open {
            if announce {
                out.push(StreamEvent::ReasoningSummaryPartAdded(SummaryPartEvent {
                    item_id: id.clone(),
                    output_index,
                    summary_index: 0,
                    part: SummaryPart::Text(TextPart::default()),
                    sequence_number: first,
                }));
            }
            summary.get_or_insert_with(String::new).push_str(&text);
            out.push(StreamEvent::ReasoningSummaryTextDelta(
                wire::SummaryDeltaEvent {
                    item_id: id.clone(),
                    output_index,
                    summary_index: 0,
                    delta: text,
                    sequence_number: second,
                },
            ));
        }
    }

    fn open_reasoning(&mut self, out: &mut Vec<StreamEvent>) {
        let id = format!("rs_vsl_{}", uuid::Uuid::new_v4().simple());
        self.added(
            Item::Reasoning(ReasoningItem {
                id: Some(id.clone()),
                summary: Vec::new(),
                content: None,
                encrypted_content: None,
                status: None,
            }),
            out,
        );
        self.open = Some(Open::Reasoning { id, summary: None });
    }

    fn added(&mut self, item: Item, out: &mut Vec<StreamEvent>) {
        let sequence_number = Some(self.next_sequence());
        out.push(StreamEvent::OutputItemAdded(OutputItemEvent {
            output_index: self.output_index(),
            item,
            sequence_number,
        }));
    }

    fn done(&mut self, item: Item, out: &mut Vec<StreamEvent>) {
        let sequence_number = Some(self.next_sequence());
        out.push(StreamEvent::OutputItemDone(OutputItemEvent {
            output_index: self.output_index(),
            item: item.clone(),
            sequence_number,
        }));
        self.output.push(item);
    }

    fn close_reasoning(&mut self, encrypted_content: Option<String>, out: &mut Vec<StreamEvent>) {
        let Some(Open::Reasoning { id, summary }) = self.open.take() else {
            return;
        };
        let output_index = self.output_index();
        let mut parts = Vec::new();
        if let Some(text) = summary {
            let sequence_number = Some(self.next_sequence());
            out.push(StreamEvent::ReasoningSummaryTextDone(
                wire::SummaryDoneEvent {
                    item_id: id.clone(),
                    output_index,
                    summary_index: 0,
                    text: text.clone(),
                    sequence_number,
                },
            ));
            let part = SummaryPart::Text(TextPart { text });
            let sequence_number = Some(self.next_sequence());
            out.push(StreamEvent::ReasoningSummaryPartDone(SummaryPartEvent {
                item_id: id.clone(),
                output_index,
                summary_index: 0,
                part: part.clone(),
                sequence_number,
            }));
            parts.push(part);
        }
        self.done(
            Item::Reasoning(ReasoningItem {
                id: Some(id),
                summary: parts,
                content: None,
                encrypted_content,
                status: None,
            }),
            out,
        );
    }

    fn close(&mut self, out: &mut Vec<StreamEvent>) {
        match self.open.take() {
            None => {}
            Some(Open::Message { id, text }) => {
                let output_index = self.output_index();
                let sequence_number = Some(self.next_sequence());
                out.push(StreamEvent::OutputTextDone(TextDoneEvent {
                    item_id: id.clone(),
                    output_index,
                    content_index: 0,
                    text: text.clone(),
                    sequence_number,
                }));
                let sequence_number = Some(self.next_sequence());
                out.push(StreamEvent::ContentPartDone(ContentPartEvent {
                    item_id: id.clone(),
                    output_index,
                    content_index: 0,
                    part: output_text(text.clone()),
                    sequence_number,
                }));
                self.done(
                    Item::Message(MessageItem {
                        id: Some(id),
                        role: Role::Assistant,
                        content: TextOr::Parts(vec![output_text(text)]),
                        status: Some(ItemStatus::Completed),
                    }),
                    out,
                );
            }
            // Reasoning that ended without its replayable form: seal the
            // text alone, so it still comes back to its provider.
            Some(open @ Open::Reasoning { .. }) => {
                self.open = Some(open);
                let content = envelope::seal(&self.source, &ir::Replay::Text);
                self.close_reasoning(Some(content), out);
            }
            Some(Open::Call {
                id,
                call_id,
                name,
                arguments,
                freeform,
            }) => {
                let output_index = self.output_index();
                let arguments = if arguments.trim().is_empty() {
                    String::from("{}")
                } else {
                    arguments
                };
                let sequence_number = Some(self.next_sequence());
                let body = if freeform {
                    let input = ir::freeform_text(&arguments).unwrap_or(arguments);
                    out.push(StreamEvent::CustomToolCallInputDone(wire::InputDoneEvent {
                        item_id: id.clone(),
                        output_index,
                        input: input.clone(),
                        sequence_number,
                    }));
                    input
                } else {
                    out.push(StreamEvent::FunctionCallArgumentsDone(
                        wire::ArgumentsDoneEvent {
                            item_id: id.clone(),
                            output_index,
                            arguments: arguments.clone(),
                            sequence_number,
                        },
                    ));
                    arguments
                };
                let item = call_item(&id, &call_id, &name, &body, freeform, ItemStatus::Completed);
                self.done(item, out);
            }
        }
    }

    /// The upstream's stream ended: the response completes, is incomplete,
    /// or failed as a truncation if the model never stopped.
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
        let incomplete = match stop {
            ir::StopReason::MaxTokens => Some(IncompleteReason::MaxOutputTokens),
            ir::StopReason::Refusal => Some(IncompleteReason::ContentFilter),
            ir::StopReason::EndTurn
            | ir::StopReason::ToolUse
            | ir::StopReason::StopSequence
            | ir::StopReason::PauseTurn => None,
        };
        let is_incomplete = incomplete.is_some();
        let mut response = self.response(if is_incomplete {
            ResponseStatus::Incomplete
        } else {
            ResponseStatus::Completed
        });
        response.usage = Some(usage(&self.usage));
        response.incomplete_details = incomplete.map(|reason| IncompleteDetails {
            reason: Some(reason),
        });
        let sequence_number = Some(self.next_sequence());
        let event = ResponseEvent {
            response,
            sequence_number,
        };
        out.push(if is_incomplete {
            StreamEvent::Incomplete(event)
        } else {
            StreamEvent::Completed(event)
        });
        self.ended = true;
    }

    fn fail(&mut self, failure: &ir::Failure, out: &mut Vec<StreamEvent>) {
        self.start(out);
        let mut response = self.response(ResponseStatus::Failed);
        response.error = Some(response_error(failure));
        let sequence_number = Some(self.next_sequence());
        out.push(StreamEvent::Failed(ResponseEvent {
            response,
            sequence_number,
        }));
        self.ended = true;
    }
}

fn output_text(text: String) -> ContentPart {
    ContentPart::OutputText(OutputText {
        text,
        annotations: Vec::new(),
    })
}

fn call_item(
    id: &str,
    call_id: &str,
    name: &str,
    body: &str,
    freeform: bool,
    status: ItemStatus,
) -> Item {
    if freeform {
        Item::CustomToolCall(CustomToolCallItem {
            id: Some(id.to_owned()),
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            input: body.to_owned(),
            status: Some(status),
        })
    } else {
        Item::FunctionCall(FunctionCallItem {
            id: Some(id.to_owned()),
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            arguments: body.to_owned(),
            status: Some(status),
        })
    }
}

/// Usage in the Responses shape, every field present (Codex and strict
/// clients read them all).
fn usage(usage: &ir::Usage) -> ResponseUsage {
    let input = usage.total_input();
    ResponseUsage {
        input_tokens: input,
        input_tokens_details: InputTokensDetails {
            cached_tokens: usage.cache_read,
        },
        output_tokens: usage.output,
        output_tokens_details: OutputTokensDetails {
            reasoning_tokens: usage.reasoning,
        },
        total_tokens: input.saturating_add(usage.output),
    }
}

/// A failure as Codex reads it: the codes it acts on, and for a rate limit
/// the wait in the words Codex parses.
fn response_error(failure: &ir::Failure) -> ResponseError {
    let (code, message) = match failure.class {
        ir::FailureClass::RateLimited => {
            let wait = failure
                .retry_after
                .unwrap_or(super::anthropic::DEFAULT_RETRY_AFTER);
            (
                ErrorCode::RateLimitExceeded,
                format!(
                    "{} Please try again in {}s.",
                    failure.message,
                    wait.as_secs().max(1)
                ),
            )
        }
        ir::FailureClass::Overloaded
        | ir::FailureClass::UpstreamError
        | ir::FailureClass::Timeout
        | ir::FailureClass::Truncated => (ErrorCode::ServerIsOverloaded, failure.message.clone()),
        ir::FailureClass::ContextTooLong => {
            (ErrorCode::ContextLengthExceeded, failure.message.clone())
        }
        // Codex shows these and does not retry them.
        ir::FailureClass::RefusedRequest => (ErrorCode::InvalidPrompt, failure.message.clone()),
        ir::FailureClass::Unauthorized => (
            ErrorCode::InvalidPrompt,
            format!(
                "the route's provider refused the node's key (check the node's models.toml and \
                 secrets.toml): {}",
                failure.message
            ),
        ),
    };
    ResponseError {
        code: Some(code),
        message,
    }
}

/// A request Vessel refuses before any provider sees it (it cannot be
/// translated, or it names another model): the body of a 400.
#[must_use]
pub fn refusal(message: String) -> ErrorBody {
    ErrorBody {
        error: ErrorDetail {
            message,
            kind: Some(ErrorType::InvalidRequest),
            code: None,
        },
    }
}

/// Keeps the response a stream ends with, for a caller that did not ask
/// for a stream.
#[derive(Debug, Default)]
pub struct Collector {
    response: Option<Response>,
}

impl Collector {
    /// Fold `event` in.
    pub fn push(&mut self, event: StreamEvent) {
        if let StreamEvent::Completed(end)
        | StreamEvent::Incomplete(end)
        | StreamEvent::Failed(end) = event
        {
            self.response = Some(end.response);
        }
    }

    /// The whole response (a failed one included), if the stream ended.
    #[must_use]
    pub fn finish(self) -> Option<Response> {
        self.response
    }
}

#[cfg(test)]
mod tests;
