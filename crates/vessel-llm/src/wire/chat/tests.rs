use super::*;
use crate::fixtures::{CHAT_PARALLEL_TOOL_CALLS, CHAT_REQUEST, events};
use crate::wire::responses::{ImageDetail, ToolName};

fn round_trip<T>(value: &T) -> T
where
    T: Serialize + serde::de::DeserializeOwned,
{
    serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
}

#[test]
fn a_request_decodes_whole() {
    let request: Request = serde_json::from_str(CHAT_REQUEST).unwrap();
    assert_eq!(round_trip(&request), request);

    assert_eq!(request.stop, Some(TextOr::Text("END".into())));
    assert_eq!(
        request.stream_options,
        Some(StreamOptions {
            include_usage: true
        })
    );
    assert_eq!(
        request.tool_choice,
        Some(StrOr::Object(NamedToolChoice::Function(NamedFunction {
            function: ToolName {
                name: "Write".into()
            }
        })))
    );

    let roles: Vec<&str> = request.messages.iter().map(Message::kind).collect();
    assert_eq!(
        roles,
        ["system", "user", "assistant", "tool", "tool", "function"]
    );
    assert!(matches!(
        request.messages.get(1),
        Some(Message::User(PromptMessage { content: TextOr::Parts(parts) }))
            if matches!(parts.get(1), Some(ContentPart::ImageUrl(ImageUrlPart { image_url })) if image_url.detail == Some(ImageDetail::Auto))
    ));
    let Some(Message::Assistant(assistant)) = request.messages.get(2) else {
        panic!("{:?}", request.messages);
    };
    assert_eq!(assistant.content, None);
    assert_eq!(assistant.reasoning_content.as_deref(), Some("Two files."));
    let ids: Vec<&str> = assistant
        .tool_calls
        .iter()
        .filter_map(|call| {
            if let ToolCall::Function(call) = call {
                Some(call.id.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ids, ["call_1", "call_2"]);
}

#[test]
fn an_assistant_turn_of_only_tool_calls_writes_null_content() {
    let message = Message::Assistant(AssistantMessage {
        content: None,
        tool_calls: vec![ToolCall::Function(FunctionCall {
            id: "call_1".into(),
            function: FunctionCallBody {
                name: "Write".into(),
                arguments: "{}".into(),
            },
        })],
        reasoning_content: None,
        reasoning: None,
    });
    assert_eq!(
        serde_json::to_string(&message).unwrap(),
        r#"{"role":"assistant","content":null,"tool_calls":[{"type":"function","id":"call_1","function":{"name":"Write","arguments":"{}"}}]}"#
    );
}

#[test]
fn a_stream_decodes_chunk_by_chunk() {
    let mut chunks = Vec::new();
    let mut done = false;
    for event in events(CHAT_PARALLEL_TOOL_CALLS) {
        assert_eq!(event.event, None);
        if event.data == DONE {
            done = true;
            continue;
        }
        let chunk: Chunk = serde_json::from_str(&event.data).unwrap();
        assert_eq!(round_trip(&chunk), chunk);
        chunks.push(chunk);
    }
    assert!(done);

    let reasoning: String = chunks
        .iter()
        .flat_map(|chunk| &chunk.choices)
        .filter_map(|choice| choice.delta.reasoning_content.as_deref())
        .collect();
    assert_eq!(reasoning, "Two files.");

    // Pieces of two calls, interleaved by index.
    let pieces: Vec<(Option<u32>, Option<&str>, &str)> = chunks
        .iter()
        .flat_map(|chunk| &chunk.choices)
        .flat_map(|choice| &choice.delta.tool_calls)
        .map(|piece| {
            (
                piece.index,
                piece.id.as_deref(),
                piece
                    .function
                    .as_ref()
                    .and_then(|function| function.arguments.as_deref())
                    .unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(pieces.first(), Some(&(Some(0), Some("call_1"), "")));
    assert_eq!(
        pieces.get(2),
        Some(&(Some(1), Some("call_2"), r#"{"file_path":"#))
    );

    let finish: Vec<&FinishReason> = chunks
        .iter()
        .flat_map(|chunk| &chunk.choices)
        .filter_map(|choice| choice.finish_reason.as_ref())
        .collect();
    assert_eq!(finish, [&FinishReason::ToolCalls]);

    let usage = chunks.last().and_then(|chunk| chunk.usage);
    assert_eq!(
        usage,
        Some(Usage {
            prompt_tokens: 200,
            completion_tokens: 50,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(128)
            }),
            completion_tokens_details: Some(CompletionTokensDetails {
                reasoning_tokens: Some(10)
            }),
        })
    );
}

#[test]
fn servers_that_differ_still_decode() {
    // No index on the piece, `reasoning` instead of `reasoning_content`,
    // an unknown finish reason, an error inside the stream.
    let chunk: Chunk = serde_json::from_str(
        r#"{"choices":[{"delta":{"reasoning":"hm","tool_calls":[{"id":"c","function":{"name":"f"}}]},
            "finish_reason":"eos"}],"error":{"code":502,"message":"Upstream died."}}"#,
    )
    .unwrap();
    let Some(choice) = chunk.choices.first() else {
        panic!("{chunk:?}");
    };
    assert_eq!(choice.delta.reasoning.as_deref(), Some("hm"));
    assert_eq!(choice.delta.tool_calls.first().and_then(|t| t.index), None);
    assert_eq!(
        choice.finish_reason,
        Some(FinishReason::Unknown("eos".into()))
    );
    assert_eq!(
        chunk.error.map(|error| error.message),
        Some("Upstream died.".into())
    );

    let body: ErrorBody =
        serde_json::from_str(r#"{"error":{"message":"Rate limited.","code":429}}"#).unwrap();
    assert_eq!(body.error.message, "Rate limited.");
}
