use std::time::Duration;

use super::*;
use crate::fixtures::RESPONSES_REQUEST;

fn decoded() -> (ir::Request, Vec<Dropped>) {
    request(serde_json::from_str(RESPONSES_REQUEST).unwrap()).unwrap()
}

#[test]
fn a_codex_request_becomes_neutral() {
    let (request, dropped) = decoded();
    assert_eq!(
        request.system,
        [
            "You are Codex, a coding agent.",
            "Sandbox: workspace-write."
        ]
    );
    assert_eq!(request.reasoning, ir::Reasoning::Effort(ir::Effort::Medium));
    assert_eq!(request.parallel_tool_calls, Some(false));
    assert_eq!(request.cache_key.as_deref().map(str::len), Some(32));
    assert_eq!(
        dropped,
        [
            Dropped::new("ghost_snapshot", "item"),
            Dropped::new("web_search", "tool")
        ]
    );
    assert!(matches!(
        request.tools.as_slice(),
        [
            ir::Tool { kind: ir::ToolKind::Function { .. }, .. },
            ir::Tool { kind: ir::ToolKind::Freeform { grammar: Some((syntax, _)) }, name, .. },
        ] if syntax == "lark" && name == "apply_patch"
    ));

    let roles: Vec<ir::Role> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            ir::Role::User,
            ir::Role::Assistant,
            ir::Role::User,
            ir::Role::Assistant,
            ir::Role::User
        ]
    );
    let Some(first) = request.messages.first() else {
        panic!("{:?}", request.messages);
    };
    assert!(matches!(
        first.parts.as_slice(),
        [ir::Part::Text(_), ir::Part::Image(ir::Image::Base64 { .. }), ir::Part::Text(untyped)]
            if untyped == "A message without its type."
    ));
    let Some(assistant) = request.messages.get(1) else {
        panic!("{:?}", request.messages);
    };
    assert!(matches!(
        assistant.parts.as_slice(),
        [
            ir::Part::Thinking(ir::Thinking { replay: ir::Replay::ResponsesEncrypted(_), source: None, text }),
            ir::Part::Text(_),
            ir::Part::ToolCall(ir::ToolCall { input: ir::ToolInput::Json(_), .. }),
        ] if text == "Planning the listing."
    ));
    assert!(matches!(
        request.messages.get(3).map(|m| m.parts.as_slice()),
        Some([ir::Part::ToolCall(ir::ToolCall { input: ir::ToolInput::Text(patch), .. })])
            if patch.starts_with("*** Begin Patch")
    ));
}

#[test]
fn a_response_kept_elsewhere_is_refused() {
    let request: wire::Request =
        serde_json::from_str(r#"{"model":"m","input":"hi","previous_response_id":"resp_1"}"#)
            .unwrap();
    assert!(matches!(
        super::request(request),
        Err(TranslateError::Unsupported(_))
    ));
}

fn tools() -> Vec<ir::Tool> {
    decoded().0.tools
}

fn encode(events: Vec<ir::Event>) -> Vec<StreamEvent> {
    let mut encoder = StreamEncoder::new("claude-sonnet-4-5", "anthropic", &tools());
    let mut out = Vec::new();
    for event in events {
        encoder.push(event, &mut out);
    }
    encoder.finish(&mut out);
    out
}

#[test]
fn the_stream_announces_and_completes_every_item() {
    let patch = "*** Begin Patch\n*** End Patch\n";
    let out = encode(vec![
        ir::Event::Thinking("Reading.".into()),
        ir::Event::ThinkingEnd(ir::Replay::AnthropicSignature("EqQB".into())),
        ir::Event::Text("Patching.".into()),
        ir::Event::ToolCallStart {
            id: "toolu_1".into(),
            name: "apply_patch".into(),
        },
        ir::Event::ToolArgs(ir::freeform_arguments(patch)),
        ir::Event::ToolCallEnd,
        ir::Event::ToolCallStart {
            id: "toolu_2".into(),
            name: "shell".into(),
        },
        ir::Event::ToolArgs(r#"{"command":["ls"]}"#.into()),
        ir::Event::ToolCallEnd,
        ir::Event::Usage(ir::Usage {
            input: 10,
            cache_read: 100,
            cache_write: 5,
            output: 20,
            reasoning: 0,
        }),
        ir::Event::Stop(ir::StopReason::ToolUse),
    ]);

    // Every event decodes as the API's own, in sequence.
    let sequence: Vec<u64> = out
        .iter()
        .map(|event| {
            let text = serde_json::to_string(event).unwrap();
            let back: StreamEvent = serde_json::from_str(&text).unwrap();
            assert_eq!(&back, event);
            sequence_of(event)
        })
        .collect();
    assert!(sequence.windows(2).all(|pair| pair.first() < pair.get(1)));
    assert!(matches!(out.first(), Some(StreamEvent::Created(_))));

    let done: Vec<&Item> = out
        .iter()
        .filter_map(|event| {
            if let StreamEvent::OutputItemDone(done) = event {
                Some(&done.item)
            } else {
                None
            }
        })
        .collect();
    let [
        Item::Reasoning(reasoning),
        Item::Message(_),
        Item::CustomToolCall(patching),
        Item::FunctionCall(listing),
    ] = done.as_slice()
    else {
        panic!("{done:?}");
    };
    assert_eq!(
        reasoning
            .encrypted_content
            .as_deref()
            .and_then(envelope::open),
        Some((
            "anthropic".into(),
            ir::Replay::AnthropicSignature("EqQB".into())
        ))
    );
    assert_eq!(patching.input, patch);
    assert_eq!(patching.call_id, "toolu_1");
    assert_eq!(listing.arguments, r#"{"command":["ls"]}"#);

    let Some(StreamEvent::Completed(end)) = out.last() else {
        panic!("{out:?}");
    };
    assert_eq!(end.response.output.len(), 4);
    assert_eq!(
        end.response
            .usage
            .map(|usage| (usage.input_tokens, usage.total_tokens)),
        Some((115, 135))
    );
    let mut collector = Collector::default();
    for event in out {
        collector.push(event);
    }
    assert_eq!(
        collector.finish().map(|response| response.status),
        Some(ResponseStatus::Completed)
    );
}

fn sequence_of(event: &StreamEvent) -> u64 {
    #[derive(serde::Deserialize)]
    struct Numbered {
        sequence_number: u64,
    }
    let text = serde_json::to_string(event).unwrap();
    serde_json::from_str::<Numbered>(&text)
        .unwrap()
        .sequence_number
}

#[test]
fn failures_carry_the_codes_codex_acts_on() {
    let mut limited = ir::Failure::new(ir::FailureClass::RateLimited, "Slow down.");
    limited.retry_after = Some(Duration::from_secs(7));
    let out = encode(vec![
        ir::Event::Text("Hi".into()),
        ir::Event::Failed(limited),
    ]);
    let Some(StreamEvent::Failed(end)) = out.last() else {
        panic!("{out:?}");
    };
    let error = end.response.error.clone().unwrap();
    assert_eq!(error.code, Some(ErrorCode::RateLimitExceeded));
    assert!(
        error.message.ends_with("Please try again in 7s."),
        "{error:?}"
    );
    let ends = out
        .iter()
        .filter(|event| {
            matches!(
                event,
                StreamEvent::Completed(_) | StreamEvent::Incomplete(_) | StreamEvent::Failed(_)
            )
        })
        .count();
    assert_eq!(ends, 1);

    let out = encode(vec![ir::Event::Text("Part".into())]);
    assert!(matches!(
        out.last(),
        Some(StreamEvent::Failed(end))
            if end.response.error.as_ref().and_then(|e| e.code.clone()) == Some(ErrorCode::ServerIsOverloaded)
    ));

    let out = encode(vec![
        ir::Event::Text("Long".into()),
        ir::Event::Stop(ir::StopReason::MaxTokens),
    ]);
    assert!(matches!(
        out.last(),
        Some(StreamEvent::Incomplete(end))
            if end.response.incomplete_details.as_ref().and_then(|d| d.reason.clone())
                == Some(IncompleteReason::MaxOutputTokens)
    ));
}

/// A conversation too long for the model reaches Codex the way `OpenAI`
/// says it, which is what Codex compacts on: `response.failed` with the
/// code `context_length_exceeded`, whether or not the answer had started.
#[test]
fn a_context_overflow_is_what_codex_compacts_on() {
    let overflow = || {
        ir::Event::Failed(ir::Failure::new(
            ir::FailureClass::ContextTooLong,
            "prompt is too long: 210000 tokens > 200000 maximum",
        ))
    };
    for events in [
        vec![overflow()],
        vec![ir::Event::Text("Part".into()), overflow()],
    ] {
        let out = encode(events);
        assert!(matches!(out.first(), Some(StreamEvent::Created(_))));
        let Some(StreamEvent::Failed(end)) = out.last() else {
            panic!("{out:?}");
        };
        assert_eq!(end.response.status, ResponseStatus::Failed);
        let text = serde_json::to_string(out.last().unwrap()).unwrap();
        assert!(
            text.contains(
                r#""error":{"code":"context_length_exceeded","message":"prompt is too long"#
            ),
            "{text}"
        );
    }
}
