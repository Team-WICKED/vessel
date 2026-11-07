use super::*;
use crate::fixtures::{ANTHROPIC_REQUEST, RESPONSES_REASONING_FUNCTION_CALL, events};
use crate::front;

fn options() -> Options {
    Options {
        provider: "openai".into(),
        sampling: false,
    }
}

fn from_claude_code() -> ir::Request {
    let (mut request, _) =
        front::anthropic::request(serde_json::from_str(ANTHROPIC_REQUEST).unwrap()).unwrap();
    request.model = "gpt-5".into();
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
fn a_claude_code_request_becomes_a_stateless_responses_request() {
    let request = request(&from_claude_code(), &options()).unwrap();
    let json = serde_json::to_string(&request).unwrap();
    let back: wire::Request = serde_json::from_str(&json).unwrap();
    assert_eq!(back, request);

    assert_eq!(request.store, Some(false));
    assert_eq!(request.include, [Include::ReasoningEncryptedContent]);
    assert_eq!(request.temperature, None, "this provider takes no sampling");
    assert_eq!(
        request.reasoning,
        Some(Reasoning {
            effort: Some(ReasoningEffort::Medium),
            summary: Some(ReasoningSummary::Auto),
        })
    );
    assert!(
        request
            .instructions
            .as_deref()
            .is_some_and(|text| text.ends_with("Mind the style guide."))
    );
    let Some(TextOr::Parts(input)) = &request.input else {
        panic!("{json}");
    };
    let kinds: Vec<&str> = input.iter().map(Item::kind).collect();
    // Anthropic's signed thinking is not this provider's: it stays behind.
    assert_eq!(
        kinds,
        [
            "message",
            "message",
            "function_call",
            "function_call_output",
            "message"
        ],
        "{json}"
    );
    assert!(!json.contains("EqQB"), "{json}");
}

#[test]
fn encrypted_reasoning_goes_back_only_to_its_provider() {
    let mut ir = from_claude_code();
    ir.messages = vec![
        ir::Message {
            role: ir::Role::User,
            parts: vec![ir::Part::Text("go".into())],
        },
        ir::Message {
            role: ir::Role::Assistant,
            parts: vec![
                ir::Part::Thinking(ir::Thinking {
                    text: "Mine.".into(),
                    replay: ir::Replay::ResponsesEncrypted("gAAAA-mine".into()),
                    source: Some("openai".into()),
                }),
                ir::Part::Thinking(ir::Thinking {
                    text: "Theirs.".into(),
                    replay: ir::Replay::ResponsesEncrypted("gAAAA-theirs".into()),
                    source: Some("azure".into()),
                }),
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
    assert!(json.contains("gAAAA-mine"), "{json}");
    assert!(!json.contains("gAAAA-theirs"), "{json}");
    assert!(!json.contains("vsl1:"), "{json}");
}

#[test]
fn the_fixture_stream_becomes_neutral_events() {
    assert_eq!(
        decode(RESPONSES_REASONING_FUNCTION_CALL),
        [
            ir::Event::Thinking("Planning the listing.".into()),
            ir::Event::ThinkingEnd(ir::Replay::ResponsesEncrypted(
                "gAAAAABexampleOpaqueReasoning".into()
            )),
            ir::Event::Text("Listing the files.".into()),
            ir::Event::ToolCallStart {
                id: "call_abc".into(),
                name: "shell".into()
            },
            ir::Event::ToolArgs(r#"{"command":["ls""#.into()),
            ir::Event::ToolArgs("]}".into()),
            ir::Event::ToolCallEnd,
            ir::Event::Usage(ir::Usage {
                input: 56,
                cache_read: 64,
                cache_write: 0,
                output: 40,
                reasoning: 16,
            }),
            ir::Event::Stop(ir::StopReason::ToolUse),
        ]
    );
}

#[test]
fn failures_and_cut_streams_end_once() {
    let failed = "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"r\",\"status\":\"failed\",\"model\":\"m\",\
                  \"error\":{\"code\":\"context_length_exceeded\",\"message\":\"Too long.\"}}}\n\n";
    assert_eq!(
        decode(failed),
        [ir::Event::Failed(ir::Failure::new(
            ir::FailureClass::ContextTooLong,
            "Too long."
        ))]
    );

    let cut: String = RESPONSES_REASONING_FUNCTION_CALL
        .split("event: response.completed")
        .next()
        .unwrap()
        .to_owned();
    assert!(matches!(
        decode(&cut).last(),
        Some(ir::Event::Failed(ir::Failure {
            class: ir::FailureClass::Truncated,
            ..
        }))
    ));

    // Arguments sent only whole, in the item's end.
    let whole = "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c\",\"name\":\"f\",\"arguments\":\"\"}}\n\n\
                 data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c\",\"name\":\"f\",\"arguments\":\"{}\"}}\n\n\
                 data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"model\":\"m\"}}\n\n";
    assert_eq!(
        decode(whole),
        [
            ir::Event::ToolCallStart {
                id: "c".into(),
                name: "f".into()
            },
            ir::Event::ToolArgs("{}".into()),
            ir::Event::ToolCallEnd,
            ir::Event::Stop(ir::StopReason::ToolUse),
        ]
    );
}
