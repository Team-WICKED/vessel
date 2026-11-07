//! The translation matrix: every request fixture out to every upstream its
//! front translates to, and every stream fixture back through every front
//! it translates for. Each pair must write what its API reads back, end an
//! answer exactly once, and keep the answer's parts in order.
//!
//! Same-API pairs (Anthropic to Anthropic, Responses to Responses) take
//! the passthrough instead and are tested there.

use crate::fixtures::{
    ANTHROPIC_ERROR_MID_STREAM, ANTHROPIC_REQUEST, ANTHROPIC_THINKING_TOOL_USE,
    CHAT_PARALLEL_TOOL_CALLS, RESPONSES_REASONING_FUNCTION_CALL, RESPONSES_REQUEST, events,
};
use crate::wire::{anthropic, responses};
use crate::{front, ir, upstream};

/// The fronts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Front {
    Anthropic,
    Responses,
}

/// The upstreams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Upstream {
    Anthropic,
    Responses,
    Chat,
}

const TRANSLATED: [(Front, Upstream); 4] = [
    (Front::Anthropic, Upstream::Responses),
    (Front::Anthropic, Upstream::Chat),
    (Front::Responses, Upstream::Anthropic),
    (Front::Responses, Upstream::Chat),
];

fn front_request(front: Front) -> ir::Request {
    match front {
        Front::Anthropic => {
            front::anthropic::request(serde_json::from_str(ANTHROPIC_REQUEST).unwrap())
                .unwrap()
                .0
        }
        Front::Responses => {
            front::responses::request(serde_json::from_str(RESPONSES_REQUEST).unwrap())
                .unwrap()
                .0
        }
    }
}

/// The request as the upstream's JSON, read back by its wire types.
fn upstream_request(request: &ir::Request, upstream: Upstream) -> String {
    match upstream {
        Upstream::Anthropic => {
            let written = upstream::anthropic::request(
                request,
                &upstream::anthropic::Options {
                    provider: "anthropic".into(),
                    default_max_tokens: 32_000,
                    adaptive_thinking: false,
                },
            )
            .unwrap();
            let text = serde_json::to_string(&written).unwrap();
            let back: anthropic::Request = serde_json::from_str(&text).unwrap();
            assert_eq!(back, written);
            text
        }
        Upstream::Responses => {
            let written = upstream::responses::request(
                request,
                &upstream::responses::Options {
                    provider: "openai".into(),
                    sampling: false,
                },
            )
            .unwrap();
            let text = serde_json::to_string(&written).unwrap();
            let back: responses::Request = serde_json::from_str(&text).unwrap();
            assert_eq!(back, written);
            text
        }
        Upstream::Chat => {
            let written = upstream::chat::request(
                request,
                &upstream::chat::Options {
                    provider: "openrouter".into(),
                    parallel_tool_calls: true,
                    max_tokens_field: upstream::chat::MaxTokensField::MaxTokens,
                    reasoning: upstream::chat::ReasoningStyle::OpenRouter,
                    replay_reasoning: true,
                },
            )
            .unwrap();
            let text = serde_json::to_string(&written).unwrap();
            let back: crate::wire::chat::Request = serde_json::from_str(&text).unwrap();
            assert_eq!(back, written);
            text
        }
    }
}

#[test]
fn every_request_reaches_every_upstream_it_translates_to() {
    for (front, upstream) in TRANSLATED {
        let mut request = front_request(front);
        request.model = "routed-model".into();
        let text = upstream_request(&request, upstream);
        assert!(text.contains("routed-model"), "{front:?} -> {upstream:?}");
        // Another API's signatures and encrypted reasoning never leave in
        // a translated request, and neither does an envelope.
        for foreign in ["EqQB", "EmwK", "gAAAA", ir::envelope::PREFIX] {
            assert!(
                !text.contains(foreign),
                "{front:?} -> {upstream:?} sent {foreign}: {text}"
            );
        }
    }
}

/// An upstream's stream fixture, read into neutral events.
fn upstream_events(upstream: Upstream, stream: &str) -> Vec<ir::Event> {
    let mut out = Vec::new();
    match upstream {
        Upstream::Anthropic => {
            let mut decoder = upstream::anthropic::StreamDecoder::new();
            for event in events(stream) {
                decoder.push(&event, &mut out);
            }
            decoder.finish(&mut out);
        }
        Upstream::Responses => {
            let mut decoder = upstream::responses::StreamDecoder::new();
            for event in events(stream) {
                decoder.push(&event, &mut out);
            }
            decoder.finish(&mut out);
        }
        Upstream::Chat => {
            let mut decoder = upstream::chat::StreamDecoder::new();
            for event in events(stream) {
                decoder.push(&event, &mut out);
            }
            decoder.finish(&mut out);
        }
    }
    let ends = out
        .iter()
        .filter(|event| matches!(event, ir::Event::Stop(_) | ir::Event::Failed(_)))
        .count();
    assert_eq!(ends, 1, "{upstream:?}: {out:?}");
    assert!(
        matches!(out.last(), Some(ir::Event::Stop(_) | ir::Event::Failed(_))),
        "{upstream:?}: {out:?}"
    );
    out
}

/// What a front wrote, as a short description of each part of the answer
/// and how it ended, read back through the front's wire types.
fn front_answer(front: Front, neutral: Vec<ir::Event>) -> (Vec<&'static str>, &'static str) {
    match front {
        Front::Anthropic => {
            let mut encoder = front::anthropic::StreamEncoder::new("routed-model", "provider");
            let mut written = Vec::new();
            for event in neutral {
                encoder.push(event, &mut written);
            }
            encoder.finish(&mut written);
            let mut collector = front::anthropic::Collector::default();
            let mut end = "none";
            for event in written {
                let text = serde_json::to_string(&event).unwrap();
                let back: anthropic::StreamEvent = serde_json::from_str(&text).unwrap();
                assert_eq!(end, "none", "an event after the end");
                end = match &back {
                    anthropic::StreamEvent::MessageStop(_) => "stop",
                    anthropic::StreamEvent::Error(_) => "error",
                    anthropic::StreamEvent::MessageStart(_)
                    | anthropic::StreamEvent::ContentBlockStart(_)
                    | anthropic::StreamEvent::ContentBlockDelta(_)
                    | anthropic::StreamEvent::ContentBlockStop(_)
                    | anthropic::StreamEvent::MessageDelta(_)
                    | anthropic::StreamEvent::Ping(_)
                    | anthropic::StreamEvent::Unknown { .. } => "none",
                };
                collector.push(back);
            }
            let parts = collector.finish().map_or_else(
                |_failed| Vec::new(),
                |message| {
                    message
                        .content
                        .iter()
                        .map(|block| match block {
                            anthropic::ContentBlock::Thinking(_) => "thinking",
                            anthropic::ContentBlock::Text(_) => "text",
                            anthropic::ContentBlock::ToolUse(_) => "call",
                            anthropic::ContentBlock::Image(_)
                            | anthropic::ContentBlock::Document(_)
                            | anthropic::ContentBlock::ToolResult(_)
                            | anthropic::ContentBlock::RedactedThinking(_)
                            | anthropic::ContentBlock::Unknown { .. } => "other",
                        })
                        .collect()
                },
            );
            (parts, end)
        }
        Front::Responses => {
            let mut encoder = front::responses::StreamEncoder::new("routed-model", "provider", &[]);
            let mut written = Vec::new();
            for event in neutral {
                encoder.push(event, &mut written);
            }
            encoder.finish(&mut written);
            let mut parts = Vec::new();
            let mut end = "none";
            for event in written {
                let text = serde_json::to_string(&event).unwrap();
                let back: responses::StreamEvent = serde_json::from_str(&text).unwrap();
                assert_eq!(end, "none", "an event after the end");
                if let responses::StreamEvent::OutputItemDone(done) = &back {
                    parts.push(match done.item {
                        responses::Item::Reasoning(_) => "thinking",
                        responses::Item::Message(_) => "text",
                        responses::Item::FunctionCall(_) | responses::Item::CustomToolCall(_) => {
                            "call"
                        }
                        responses::Item::FunctionCallOutput(_)
                        | responses::Item::CustomToolCallOutput(_)
                        | responses::Item::Unknown { .. } => "other",
                    });
                }
                if matches!(
                    back,
                    responses::StreamEvent::Completed(_) | responses::StreamEvent::Incomplete(_)
                ) {
                    end = "stop";
                } else if matches!(back, responses::StreamEvent::Failed(_)) {
                    end = "error";
                }
            }
            (parts, end)
        }
    }
}

#[test]
fn every_stream_comes_back_through_every_front_it_translates_for() {
    let cases: [(Upstream, &str, &[&str], &str); 4] = [
        (
            Upstream::Chat,
            CHAT_PARALLEL_TOOL_CALLS,
            &["thinking", "text", "call", "call"],
            "stop",
        ),
        (
            Upstream::Responses,
            RESPONSES_REASONING_FUNCTION_CALL,
            &["thinking", "text", "call"],
            "stop",
        ),
        (
            Upstream::Anthropic,
            ANTHROPIC_THINKING_TOOL_USE,
            &["thinking", "text", "call"],
            "stop",
        ),
        (
            Upstream::Anthropic,
            ANTHROPIC_ERROR_MID_STREAM,
            &[],
            "error",
        ),
    ];
    for (upstream, stream, parts, end) in cases {
        for (front, to) in TRANSLATED {
            if to != upstream {
                continue;
            }
            let (written, ended) = front_answer(front, upstream_events(upstream, stream));
            assert_eq!(ended, end, "{upstream:?} -> {front:?}");
            if end == "stop" {
                assert_eq!(written, parts, "{upstream:?} -> {front:?}");
            }
        }
    }
}
