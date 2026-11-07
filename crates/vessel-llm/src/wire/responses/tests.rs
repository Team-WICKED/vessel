use super::*;
use crate::fixtures::{RESPONSES_REASONING_FUNCTION_CALL, RESPONSES_REQUEST, events};

fn round_trip<T>(value: &T) -> T
where
    T: Serialize + serde::de::DeserializeOwned,
{
    serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
}

#[test]
fn a_codex_request_decodes_whole() {
    let request: Request = serde_json::from_str(RESPONSES_REQUEST).unwrap();
    assert_eq!(round_trip(&request), request);

    assert_eq!(request.tool_choice, Some(StrOr::Str(ToolChoiceMode::Auto)));
    assert_eq!(request.include, [Include::ReasoningEncryptedContent]);
    assert_eq!(
        request.reasoning,
        Some(Reasoning {
            effort: Some(ReasoningEffort::Medium),
            summary: Some(ReasoningSummary::Auto),
        })
    );
    let tools: Vec<&str> = request.tools.iter().map(Tool::kind).collect();
    assert_eq!(tools, ["function", "custom", "web_search"]);
    assert!(matches!(
        request.tools.get(1),
        Some(Tool::Custom(CustomTool { format: Some(CustomFormat::Grammar(grammar)), .. }))
            if grammar.syntax == GrammarSyntax::Lark
    ));

    let Some(TextOr::Parts(input)) = &request.input else {
        panic!("{:?}", request.input);
    };
    let kinds: Vec<&str> = input.iter().map(Item::kind).collect();
    assert_eq!(
        kinds,
        [
            "message",
            "message",
            "message",
            "reasoning",
            "message",
            "function_call",
            "function_call_output",
            "custom_tool_call",
            "custom_tool_call_output",
            "ghost_snapshot",
        ]
    );
    // The message without a `type` is a message all the same.
    assert!(matches!(
        input.get(2),
        Some(Item::Message(MessageItem { role: Role::User, content: TextOr::Text(text), .. }))
            if text == "A message without its type."
    ));
    assert!(matches!(
        input.get(3),
        Some(Item::Reasoning(ReasoningItem {
            id: None,
            encrypted_content: Some(_),
            ..
        }))
    ));
    assert!(matches!(
        input.get(5),
        Some(Item::FunctionCall(call)) if call.call_id == "call_abc" && call.arguments == r#"{"command":["ls"]}"#
    ));
}

#[test]
fn a_stream_decodes_event_by_event() {
    let events: Vec<StreamEvent> = events(RESPONSES_REASONING_FUNCTION_CALL)
        .into_iter()
        .map(|event| {
            let decoded: StreamEvent = serde_json::from_str(&event.data).unwrap();
            assert_eq!(event.event.as_deref(), Some(decoded.kind()));
            assert!(
                !matches!(decoded, StreamEvent::Unknown { .. }),
                "{decoded:?}"
            );
            assert_eq!(round_trip(&decoded), decoded);
            decoded
        })
        .collect();

    let Some(StreamEvent::OutputItemDone(reasoning)) = events.get(7) else {
        panic!("{events:?}");
    };
    assert!(matches!(
        &reasoning.item,
        Item::Reasoning(ReasoningItem { encrypted_content: Some(opaque), .. })
            if opaque.starts_with("gAAAA")
    ));

    let Some(StreamEvent::Completed(completed)) = events.last() else {
        panic!("{events:?}");
    };
    assert_eq!(completed.response.status, ResponseStatus::Completed);
    assert_eq!(
        completed.response.usage,
        Some(ResponseUsage {
            input_tokens: 120,
            input_tokens_details: InputTokensDetails { cached_tokens: 64 },
            output_tokens: 40,
            output_tokens_details: OutputTokensDetails {
                reasoning_tokens: 16
            },
            total_tokens: 160,
        })
    );
}

#[test]
fn a_response_writes_its_nulls() {
    let created = StreamEvent::Created(ResponseEvent {
        response: Response {
            id: "resp_1".into(),
            object: ResponseTag::Response,
            status: ResponseStatus::InProgress,
            model: "m".into(),
            output: Vec::new(),
            usage: None,
            error: None,
            incomplete_details: None,
        },
        sequence_number: Some(0),
    });
    assert_eq!(
        serde_json::to_string(&created).unwrap(),
        r#"{"type":"response.created","response":{"id":"resp_1","object":"response","status":"in_progress","model":"m","output":[],"usage":null,"error":null,"incomplete_details":null},"sequence_number":0}"#
    );
}

#[test]
fn tool_choice_takes_a_mode_or_a_tool() {
    let named: ToolChoice = serde_json::from_str(r#"{"type":"function","name":"shell"}"#).unwrap();
    assert_eq!(
        named,
        StrOr::Object(NamedToolChoice::Function(ToolName {
            name: "shell".into()
        }))
    );
    let mode: ToolChoice = serde_json::from_str(r#""required""#).unwrap();
    assert_eq!(mode, StrOr::Str(ToolChoiceMode::Required));
    assert_eq!(serde_json::to_string(&mode).unwrap(), r#""required""#);
    assert!(serde_json::from_str::<ToolChoice>("7").is_err());
}

#[test]
fn errors_name_their_class() {
    let failed: StreamEvent = serde_json::from_str(
        r#"{"type":"response.failed","response":{"id":"resp_2","status":"failed","model":"m",
            "error":{"code":"context_length_exceeded","message":"Too long."}}}"#,
    )
    .unwrap();
    let StreamEvent::Failed(failed) = failed else {
        panic!("{failed:?}");
    };
    assert_eq!(
        failed.response.error.and_then(|error| error.code),
        Some(ErrorCode::ContextLengthExceeded)
    );

    let body: ErrorBody = serde_json::from_str(
        r#"{"error":{"message":"Slow down.","type":"rate_limit_error","param":null,"code":"rate_limit_exceeded"}}"#,
    )
    .unwrap();
    assert_eq!(body.error.kind, Some(ErrorType::RateLimit));
    assert_eq!(body.error.code, Some(ErrorCode::RateLimitExceeded));
}
