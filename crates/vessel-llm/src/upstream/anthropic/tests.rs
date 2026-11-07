use super::*;
use crate::fixtures::{
    ANTHROPIC_ERROR_MID_STREAM, ANTHROPIC_THINKING_TOOL_USE, RESPONSES_REQUEST, events,
};
use crate::front;
use crate::ir::envelope;

fn options() -> Options {
    Options {
        provider: "anthropic".into(),
        default_max_tokens: 32_000,
        adaptive_thinking: false,
    }
}

fn from_codex() -> ir::Request {
    let (mut request, _) =
        front::responses::request(serde_json::from_str(RESPONSES_REQUEST).unwrap()).unwrap();
    request.model = "claude-sonnet-4-5".into();
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
fn a_codex_request_becomes_an_anthropic_request() {
    let request = request(&from_codex(), &options()).unwrap();
    let json = serde_json::to_string(&request).unwrap();
    let back: wire::Request = serde_json::from_str(&json).unwrap();
    assert_eq!(back, request);

    assert_eq!(request.max_tokens, Some(32_000));
    assert_eq!(
        request.thinking,
        Some(wire::Thinking::Enabled(ThinkingBudget {
            budget_tokens: 16_384
        }))
    );
    assert_eq!(request.temperature, None);
    // The system prompt's end is a cache breakpoint, and so is the
    // conversation's.
    assert!(matches!(
        &request.system,
        Some(TextOr::Parts(blocks))
            if matches!(blocks.last(), Some(ContentBlock::Text(TextBlock { cache_control: Some(_), .. })))
    ));
    assert!(matches!(
        request.messages.last().map(|m| &m.content),
        Some(TextOr::Parts(blocks))
            if matches!(blocks.last(), Some(ContentBlock::ToolResult(ToolResultBlock { cache_control: Some(_), .. })))
    ));
    let roles: Vec<Role> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User
        ]
    );
    // `OpenAI`'s encrypted reasoning stays behind; the free-form call is
    // a call of the function form.
    assert!(!json.contains("gAAAA"), "{json}");
    let Some(TextOr::Parts(patching)) = request.messages.get(3).map(|m| &m.content) else {
        panic!("{json}");
    };
    let [ContentBlock::ToolUse(call)] = patching.as_slice() else {
        panic!("{json}");
    };
    assert_eq!(call.id, "call_def");
    assert_eq!(
        ir::freeform_text(call.input.as_text()).as_deref(),
        Some("*** Begin Patch\n*** End Patch\n")
    );
    assert!(matches!(
        request.tools.get(1),
        Some(wire::Tool::Custom(tool)) if tool.input_schema.as_text().contains("lark grammar")
    ));
}

/// ADR 0015, decision 8, the mixed history: thinking from every source in
/// one conversation; only Anthropic's own reaches Anthropic, and no
/// envelope does.
#[test]
fn only_anthropic_s_own_thinking_reaches_it() {
    let thinking = |replay: ir::Replay, source: Option<&str>| {
        ir::Part::Thinking(ir::Thinking {
            text: "thought".into(),
            replay,
            source: source.map(str::to_owned),
        })
    };
    let mut ir = from_codex();
    ir.messages = vec![
        ir::Message {
            role: ir::Role::User,
            parts: vec![ir::Part::Text("go".into())],
        },
        ir::Message {
            role: ir::Role::Assistant,
            parts: vec![
                thinking(ir::Replay::AnthropicSignature("native-sig".into()), None),
                thinking(
                    ir::Replay::AnthropicSignature("own-sig".into()),
                    Some("anthropic"),
                ),
                thinking(
                    ir::Replay::AnthropicRedacted("own-redacted".into()),
                    Some("anthropic"),
                ),
                thinking(
                    ir::Replay::AnthropicSignature("other-anthropic-sig".into()),
                    Some("bedrock"),
                ),
                thinking(ir::Replay::Text, Some("openrouter")),
                thinking(
                    ir::Replay::ResponsesEncrypted("gAAAA".into()),
                    Some("openai"),
                ),
                thinking(ir::Replay::AnthropicSignature(String::new()), None),
                ir::Part::Text("ok".into()),
            ],
        },
        ir::Message {
            role: ir::Role::User,
            parts: vec![ir::Part::Text("more".into())],
        },
    ];
    let request = request(&ir, &options()).unwrap();
    let json = serde_json::to_string(&request).unwrap();
    for kept in ["native-sig", "own-sig", "own-redacted"] {
        assert!(json.contains(kept), "{kept} missing: {json}");
    }
    for dropped in ["other-anthropic-sig", "gAAAA", envelope::PREFIX] {
        assert!(!json.contains(dropped), "{dropped} sent: {json}");
    }
    let Some(TextOr::Parts(blocks)) = request.messages.get(1).map(|m| &m.content) else {
        panic!("{json}");
    };
    let kinds: Vec<&str> = blocks.iter().map(ContentBlock::kind).collect();
    assert_eq!(kinds, ["thinking", "thinking", "redacted_thinking", "text"]);

    // Without thinking in the request, no thinking block is sent at all.
    ir.reasoning = ir::Reasoning::Off;
    let json = serde_json::to_string(&super::request(&ir, &options()).unwrap()).unwrap();
    assert!(!json.contains("native-sig"), "{json}");
}

#[test]
fn the_budget_fits_under_max_tokens_and_forced_choices_yield_to_thinking() {
    let mut ir = from_codex();
    ir.reasoning = ir::Reasoning::Budget(20_000);
    ir.max_output_tokens = Some(8000);
    ir.tool_choice = ir::ToolChoice::Any;
    let request = request(&ir, &options()).unwrap();
    assert_eq!(request.max_tokens, Some(21_024));
    assert!(matches!(
        request.tool_choice,
        Some(wire::ToolChoice::Auto(_))
    ));

    let adaptive = Options {
        adaptive_thinking: true,
        ..options()
    };
    ir.reasoning = ir::Reasoning::Effort(ir::Effort::Max);
    let request = super::request(&ir, &adaptive).unwrap();
    assert_eq!(request.thinking, Some(wire::Thinking::Adaptive(Nothing {})));
    assert_eq!(
        request.output_config.and_then(|config| config.effort),
        Some(wire::Effort::Max)
    );
}

#[test]
fn the_fixture_streams_become_neutral_events() {
    assert_eq!(
        decode(ANTHROPIC_THINKING_TOOL_USE),
        [
            ir::Event::Thinking("The user wants a file.".into()),
            ir::Event::ThinkingEnd(ir::Replay::AnthropicSignature(
                "EqQBCkYIARgCIkBexampleSignatureOnly".into()
            )),
            ir::Event::Text("I'll create it.".into()),
            ir::Event::ToolCallStart {
                id: "toolu_01A".into(),
                name: "Write".into()
            },
            ir::Event::ToolArgs(r#"{"file_path": "hello.txt","#.into()),
            ir::Event::ToolArgs(r#" "content": "hi\n"}"#.into()),
            ir::Event::ToolCallEnd,
            ir::Event::Usage(ir::Usage {
                input: 25,
                cache_read: 100,
                cache_write: 0,
                output: 89,
                reasoning: 0,
            }),
            ir::Event::Stop(ir::StopReason::ToolUse),
        ]
    );
    assert_eq!(
        decode(ANTHROPIC_ERROR_MID_STREAM),
        [
            ir::Event::Text("Part".into()),
            ir::Event::Failed(ir::Failure::new(ir::FailureClass::Overloaded, "Overloaded")),
        ]
    );
    let cut = ANTHROPIC_THINKING_TOOL_USE
        .split("event: message_stop")
        .next()
        .unwrap();
    assert!(matches!(
        decode(cut).last(),
        Some(ir::Event::Failed(ir::Failure {
            class: ir::FailureClass::Truncated,
            ..
        }))
    ));
}

#[test]
fn errors_map_to_their_classes() {
    let class = |kind: ErrorKind, message: &str| {
        failure(&ErrorDetail {
            kind,
            message: message.into(),
        })
        .class
    };
    assert_eq!(
        class(
            ErrorKind::InvalidRequest,
            "prompt is too long: 210000 tokens"
        ),
        ir::FailureClass::ContextTooLong
    );
    assert_eq!(
        class(ErrorKind::InvalidRequest, "bad"),
        ir::FailureClass::RefusedRequest
    );
    assert_eq!(
        class(ErrorKind::Authentication, "bad key"),
        ir::FailureClass::Unauthorized
    );
    assert_eq!(
        class(ErrorKind::RateLimit, "slow"),
        ir::FailureClass::RateLimited
    );
}
