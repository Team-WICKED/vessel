use super::*;
use crate::fixtures::{
    ANTHROPIC_ERROR_MID_STREAM, ANTHROPIC_REQUEST, ANTHROPIC_THINKING_TOOL_USE, events,
};

/// Decode, encode, decode again: the second decode equals the first.
fn round_trip<T>(value: &T) -> T
where
    T: Serialize + serde::de::DeserializeOwned,
{
    serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
}

fn stream(text: &str) -> Vec<StreamEvent> {
    events(text)
        .into_iter()
        .map(|event| {
            let decoded: StreamEvent = serde_json::from_str(&event.data).unwrap();
            assert_eq!(event.event.as_deref(), Some(decoded.kind()));
            assert_eq!(round_trip(&decoded), decoded);
            decoded
        })
        .collect()
}

#[test]
fn a_claude_code_request_decodes_whole() {
    let request: Request = serde_json::from_str(ANTHROPIC_REQUEST).unwrap();
    assert_eq!(round_trip(&request), request);

    assert_eq!(request.model, "claude-sonnet-4-5");
    assert_eq!(request.max_tokens, Some(32_000));
    assert_eq!(request.stream, Some(true));
    assert_eq!(
        request.thinking,
        Some(Thinking::Enabled(ThinkingBudget {
            budget_tokens: 8000
        }))
    );
    assert_eq!(
        request.tool_choice,
        Some(ToolChoice::Auto(ToolChoiceOptions::default()))
    );

    // A client tool with and without its `type`, and a server tool.
    let kinds: Vec<&str> = request.tools.iter().map(Tool::kind).collect();
    assert_eq!(kinds, ["custom", "custom", "web_search_20250305"]);
    let Some(Tool::Custom(read)) = request.tools.get(1) else {
        panic!("{:?}", request.tools);
    };
    assert_eq!(
        read.cache_control.as_ref().and_then(|c| c.ttl.clone()),
        Some(CacheTtl::OneHour)
    );

    let roles: Vec<Role> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::User,
            Role::System,
            Role::Assistant,
            Role::User
        ]
    );

    let Some(TextOr::Parts(assistant)) = request.messages.get(3).map(|m| &m.content) else {
        panic!("{:?}", request.messages);
    };
    let kinds: Vec<&str> = assistant.iter().map(ContentBlock::kind).collect();
    assert_eq!(
        kinds,
        [
            "thinking",
            "redacted_thinking",
            "text",
            "tool_use",
            "server_tool_use"
        ]
    );
    let Some(ContentBlock::ToolUse(call)) = assistant.get(3) else {
        panic!("{assistant:?}");
    };
    assert_eq!(call.id, "toolu_01A");
    assert_eq!(
        call.input.as_text(),
        r#"{"file_path":"hello.txt","content":"hi\n"}"#
    );
    assert!(matches!(
        assistant.get(4),
        Some(ContentBlock::Unknown { kind, .. }) if kind == "server_tool_use"
    ));

    let Some(TextOr::Parts(results)) = request.messages.get(4).map(|m| &m.content) else {
        panic!("{:?}", request.messages);
    };
    let Some(ContentBlock::ToolResult(failed)) = results.get(1) else {
        panic!("{results:?}");
    };
    assert_eq!(failed.is_error, Some(true));
    assert!(matches!(
        &failed.content,
        Some(TextOr::Parts(blocks)) if matches!(blocks.as_slice(), [ContentBlock::Text(_)])
    ));
}

#[test]
fn adaptive_thinking_takes_its_effort_from_output_config() {
    let request: Request = serde_json::from_str(
        r#"{"model":"m","max_tokens":1,"messages":[],
            "thinking":{"type":"adaptive"},"output_config":{"effort":"max"}}"#,
    )
    .unwrap();
    assert_eq!(request.thinking, Some(Thinking::Adaptive(Nothing {})));
    assert_eq!(
        request.output_config.and_then(|config| config.effort),
        Some(Effort::Max)
    );
}

#[test]
fn a_stream_decodes_event_by_event() {
    let events = stream(ANTHROPIC_THINKING_TOOL_USE);
    let Some(StreamEvent::MessageStart(start)) = events.first() else {
        panic!("{events:?}");
    };
    assert_eq!(start.message.stop_reason, None);
    assert_eq!(start.message.usage.cache_read_input_tokens, Some(100));

    let deltas: Vec<&Delta> = events
        .iter()
        .filter_map(|event| {
            if let StreamEvent::ContentBlockDelta(delta) = event {
                Some(&delta.delta)
            } else {
                None
            }
        })
        .collect();
    assert!(matches!(
        deltas.get(1),
        Some(Delta::Signature(SignatureDelta { signature })) if signature.starts_with("EqQB")
    ));
    let arguments: String = deltas
        .iter()
        .filter_map(|delta| {
            if let Delta::InputJson(piece) = delta {
                Some(piece.partial_json.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        arguments,
        r#"{"file_path": "hello.txt", "content": "hi\n"}"#
    );

    let Some(StreamEvent::MessageDelta(end)) = events.get(events.len() - 2) else {
        panic!("{events:?}");
    };
    assert_eq!(end.delta.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(end.usage.output_tokens, Some(89));
    assert!(matches!(events.last(), Some(StreamEvent::MessageStop(_))));
}

#[test]
fn an_error_event_names_its_class() {
    let events = stream(ANTHROPIC_ERROR_MID_STREAM);
    let Some(StreamEvent::Error(error)) = events.last() else {
        panic!("{events:?}");
    };
    assert_eq!(error.error.kind, ErrorKind::Overloaded);

    let body: ErrorBody = serde_json::from_str(
        r#"{"type":"error","error":{"type":"some_new_error","message":"x"},"request_id":"req_1"}"#,
    )
    .unwrap();
    assert_eq!(body.error.kind, ErrorKind::Unknown("some_new_error".into()));
}

#[test]
fn a_message_start_writes_its_nulls() {
    let start = StreamEvent::MessageStart(MessageStart {
        message: Message {
            id: "msg_1".into(),
            tag: MessageTag::Message,
            role: Role::Assistant,
            model: "m".into(),
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: Usage {
                input_tokens: Some(3),
                output_tokens: Some(1),
                ..Usage::default()
            },
        },
    });
    assert_eq!(
        serde_json::to_string(&start).unwrap(),
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":1}}}"#
    );
}

#[test]
fn unknown_kinds_survive_and_known_ones_must_be_whole() {
    let event: StreamEvent = serde_json::from_str(
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#,
    )
    .unwrap();
    let StreamEvent::ContentBlockStart(start) = &event else {
        panic!("{event:?}");
    };
    assert_eq!(start.content_block.kind(), "server_tool_use");
    assert_eq!(round_trip(&event), event);

    let brand_new: StreamEvent =
        serde_json::from_str(r#"{"type":"message_pause","reason":"later"}"#).unwrap();
    assert_eq!(brand_new.kind(), "message_pause");

    let error = serde_json::from_str::<ContentBlock>(r#"{"type":"tool_use","name":"Write"}"#)
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing field `id`"), "{error}");
}
