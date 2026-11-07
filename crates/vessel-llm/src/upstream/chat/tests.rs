use super::*;
use crate::fixtures::{ANTHROPIC_REQUEST, CHAT_PARALLEL_TOOL_CALLS, events};
use crate::front;
use crate::ir::envelope;

fn options() -> Options {
    Options {
        provider: "openrouter".into(),
        parallel_tool_calls: true,
        max_tokens_field: MaxTokensField::MaxTokens,
        reasoning: ReasoningStyle::OpenRouter,
        replay_reasoning: true,
    }
}

fn from_claude_code() -> ir::Request {
    let (mut request, _) =
        front::anthropic::request(serde_json::from_str(ANTHROPIC_REQUEST).unwrap()).unwrap();
    // The fixture's document is plain text; images by URL travel too.
    request.model = "qwen/qwen3-coder".into();
    request
}

fn decode(stream: &str) -> Vec<ir::Event> {
    let mut decoder = StreamDecoder::new();
    let mut out = Vec::new();
    for event in events(stream) {
        decoder.push(&event, &mut out);
    }
    decoder.finish(&mut out);
    out
}

#[test]
fn a_claude_code_request_becomes_a_chat_request() {
    let request = request(&from_claude_code(), &options()).unwrap();
    let json = serde_json::to_string(&request).unwrap();
    // Round trip through the wire types: what is written can be read.
    let back: wire::Request = serde_json::from_str(&json).unwrap();
    assert_eq!(back, request);

    assert_eq!(request.model, "qwen/qwen3-coder");
    assert_eq!(request.max_tokens, Some(32_000));
    assert_eq!(request.stream, Some(true));
    assert_eq!(request.parallel_tool_calls, Some(true));
    assert_eq!(
        request.reasoning,
        Some(ReasoningConfig {
            effort: Some(ReasoningEffort::Medium),
            max_tokens: None,
            exclude: None,
        })
    );

    let roles: Vec<&str> = request.messages.iter().map(Message::kind).collect();
    assert_eq!(
        roles,
        ["system", "user", "assistant", "tool", "user"],
        "{json}"
    );
    let Some(Message::System(system)) = request.messages.first() else {
        panic!("{json}");
    };
    assert_eq!(
        system.content,
        TextOr::Text(
            "You are a coding agent.\n\nWork in /workspace.\n\nMind the style guide.".into()
        )
    );
    // Anthropic's thinking is not this provider's: it stays behind.
    let Some(Message::Assistant(assistant)) = request.messages.get(2) else {
        panic!("{json}");
    };
    assert_eq!(assistant.reasoning_content, None);
    assert_eq!(
        assistant.content,
        Some(TextOr::Text("I'll create it.".into()))
    );
    assert!(matches!(
        assistant.tool_calls.as_slice(),
        [ToolCall::Function(FunctionCall { id, function })]
            if id == "toolu_01A" && function.arguments == r#"{"file_path":"hello.txt","content":"hi\n"}"#
    ));
    assert!(matches!(
        request.messages.get(3),
        Some(Message::Tool(ToolMessage { tool_call_id, .. })) if tool_call_id == "toolu_01A"
    ));
}

#[test]
fn reasoning_goes_back_only_to_the_provider_that_made_it() {
    let thinking = |source: &str| {
        ir::Part::Thinking(ir::Thinking {
            text: "Two files.".into(),
            replay: ir::Replay::Text,
            source: Some(source.into()),
        })
    };
    let mut ir = from_claude_code();
    ir.messages = vec![
        ir::Message {
            role: ir::Role::User,
            parts: vec![ir::Part::Text("go".into())],
        },
        ir::Message {
            role: ir::Role::Assistant,
            parts: vec![
                thinking("openrouter"),
                thinking("other"),
                ir::Part::Text("ok".into()),
            ],
        },
        ir::Message {
            role: ir::Role::User,
            parts: vec![ir::Part::Text("more".into())],
        },
    ];
    let request = request(&ir, &options()).unwrap();
    assert!(matches!(
        request.messages.get(2),
        Some(Message::Assistant(AssistantMessage { reasoning_content: Some(text), .. }))
            if text == "Two files."
    ));

    let quiet = Options {
        replay_reasoning: false,
        ..options()
    };
    let request = super::request(&ir, &quiet).unwrap();
    assert!(matches!(
        request.messages.get(2),
        Some(Message::Assistant(AssistantMessage {
            reasoning_content: None,
            ..
        }))
    ));
}

#[test]
fn a_pdf_is_refused_and_a_tool_only_turn_has_empty_content() {
    let mut ir = from_claude_code();
    ir.messages = vec![ir::Message {
        role: ir::Role::User,
        parts: vec![ir::Part::Document(ir::Document::Pdf {
            data: "JVBERi0=".into(),
            title: None,
        })],
    }];
    assert!(matches!(
        request(&ir, &options()),
        Err(TranslateError::Unsupported(_))
    ));

    ir.messages = vec![
        ir::Message {
            role: ir::Role::User,
            parts: vec![ir::Part::Text("go".into())],
        },
        ir::Message {
            role: ir::Role::Assistant,
            parts: vec![ir::Part::ToolCall(ir::ToolCall {
                id: String::new(),
                name: "apply_patch".into(),
                input: ir::ToolInput::Text("*** Begin Patch".into()),
            })],
        },
    ];
    let request = request(&ir, &options()).unwrap();
    let Some(Message::Assistant(assistant)) = request.messages.get(2) else {
        panic!("{request:?}");
    };
    assert_eq!(assistant.content, Some(TextOr::Text(String::new())));
    let [ToolCall::Function(call)] = assistant.tool_calls.as_slice() else {
        panic!("{assistant:?}");
    };
    assert!(call.id.starts_with("call_vsl_"));
    assert_eq!(
        ir::freeform_text(&call.function.arguments).as_deref(),
        Some("*** Begin Patch")
    );
}

#[test]
fn the_fixture_stream_becomes_neutral_events() {
    let out = decode(CHAT_PARALLEL_TOOL_CALLS);
    assert_eq!(
        out,
        [
            ir::Event::Thinking("Two files.".into()),
            ir::Event::ThinkingEnd(ir::Replay::Text),
            ir::Event::Text("Writing both.".into()),
            ir::Event::ToolCallStart {
                id: "call_1".into(),
                name: "Write".into()
            },
            ir::Event::ToolArgs(r#"{"file_path":"a.txt","content":"a"}"#.into()),
            ir::Event::ToolCallEnd,
            ir::Event::ToolCallStart {
                id: "call_2".into(),
                name: "Write".into()
            },
            ir::Event::ToolArgs(r#"{"file_path":"b.txt","content":"b"}"#.into()),
            ir::Event::ToolCallEnd,
            ir::Event::Usage(ir::Usage {
                input: 72,
                cache_read: 128,
                cache_write: 0,
                output: 50,
                reasoning: 10,
            }),
            ir::Event::Stop(ir::StopReason::ToolUse),
        ]
    );
}

#[test]
fn cut_short_or_failing_streams_end_in_one_failure() {
    // Without its last three events (finish, usage, `[DONE]`) and the
    // empty piece after the last blank line.
    let pieces: Vec<&str> = CHAT_PARALLEL_TOOL_CALLS.split("\n\n").collect();
    let kept: Vec<&str> = pieces
        .iter()
        .copied()
        .take(pieces.len().saturating_sub(4))
        .collect();
    let cut = kept.join("\n\n");
    let out = decode(&cut);
    assert!(matches!(
        out.last(),
        Some(ir::Event::Failed(ir::Failure {
            class: ir::FailureClass::Truncated,
            ..
        }))
    ));
    assert!(!out.iter().any(|event| matches!(event, ir::Event::Stop(_))));

    let failing = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
                   data: {\"error\":{\"code\":502,\"message\":\"Provider died.\"}}\n\n\
                   data: [DONE]\n\n";
    let out = decode(failing);
    assert_eq!(
        out,
        [
            ir::Event::Text("Hi".into()),
            ir::Event::Failed(ir::Failure::new(
                ir::FailureClass::UpstreamError,
                "Provider died."
            )),
        ]
    );

    let garbage = decode("data: {not json\n\n");
    assert!(matches!(
        garbage.as_slice(),
        [ir::Event::Failed(ir::Failure {
            class: ir::FailureClass::UpstreamError,
            ..
        })]
    ));
}

#[test]
fn done_alone_ends_the_answer_and_empty_arguments_are_an_object() {
    let stream = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"c\",\"function\":{\"name\":\"List\"}}]}}]}\n\n\
                  data: [DONE]\n\n";
    assert_eq!(
        decode(stream),
        [
            ir::Event::ToolCallStart {
                id: "c".into(),
                name: "List".into()
            },
            ir::Event::ToolArgs("{}".into()),
            ir::Event::ToolCallEnd,
            ir::Event::Stop(ir::StopReason::ToolUse),
        ]
    );
}

/// The pair the manual smoke runs: Claude Code's request through the
/// Anthropic front and out to a Chat Completions provider, and the
/// provider's stream back to Claude Code's events, with nothing lost.
#[test]
fn claude_code_to_chat_and_back() {
    let out = decode(CHAT_PARALLEL_TOOL_CALLS);
    let mut encoder = front::anthropic::StreamEncoder::new("qwen/qwen3-coder", "openrouter");
    let mut written = Vec::new();
    for event in out {
        encoder.push(event, &mut written);
    }
    encoder.finish(&mut written);

    // What is written decodes as Anthropic's own events, ends once, and
    // folds into a whole message.
    let mut collector = front::anthropic::Collector::default();
    for event in written {
        let text = serde_json::to_string(&event).unwrap();
        let back: crate::wire::anthropic::StreamEvent = serde_json::from_str(&text).unwrap();
        collector.push(back);
    }
    let message = collector.finish().unwrap();
    assert_eq!(
        message.stop_reason,
        Some(crate::wire::anthropic::StopReason::ToolUse)
    );
    let kinds: Vec<&str> = message
        .content
        .iter()
        .map(crate::wire::anthropic::ContentBlock::kind)
        .collect();
    assert_eq!(kinds, ["thinking", "text", "tool_use", "tool_use"]);
    let Some(crate::wire::anthropic::ContentBlock::Thinking(thinking)) = message.content.first()
    else {
        panic!("{message:?}");
    };
    assert_eq!(
        envelope::open(&thinking.signature),
        Some(("openrouter".into(), ir::Replay::Text))
    );
    assert_eq!(message.usage.output_tokens, Some(50));
}
