use super::*;
use crate::fixtures::ANTHROPIC_REQUEST;

fn decoded() -> (ir::Request, Vec<Dropped>) {
    request(serde_json::from_str(ANTHROPIC_REQUEST).unwrap()).unwrap()
}

#[test]
fn a_claude_code_request_becomes_neutral() {
    let (request, dropped) = decoded();
    assert_eq!(request.model, "claude-sonnet-4-5");
    // Both system blocks and the `role: "system"` message.
    assert_eq!(
        request.system,
        [
            "You are a coding agent.",
            "Work in /workspace.",
            "Mind the style guide."
        ]
    );
    assert_eq!(request.reasoning, ir::Reasoning::Budget(8000));
    assert_eq!(request.max_output_tokens, Some(32_000));
    assert_eq!(
        request.cache_key.as_deref().map(str::len),
        Some(32),
        "hashed, never the caller's id as is"
    );
    let tools: Vec<&str> = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(tools, ["Write", "Read"]);
    assert_eq!(
        dropped,
        [
            Dropped::new("server_tool_use", "content block"),
            Dropped::new("web_search_20250305", "tool"),
        ]
    );

    let roles: Vec<ir::Role> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [ir::Role::User, ir::Role::Assistant, ir::Role::User]);
    let Some(assistant) = request.messages.get(1) else {
        panic!("{:?}", request.messages);
    };
    assert!(matches!(
        assistant.parts.as_slice(),
        [
            ir::Part::Thinking(ir::Thinking { replay: ir::Replay::AnthropicSignature(_), source: None, .. }),
            ir::Part::Thinking(ir::Thinking { replay: ir::Replay::AnthropicRedacted(_), .. }),
            ir::Part::Text(_),
            ir::Part::ToolCall(ir::ToolCall { id, input: ir::ToolInput::Json(input), .. }),
        ] if id == "toolu_01A" && input == r#"{"file_path":"hello.txt","content":"hi\n"}"#
    ));
    // `toolu_01B`'s result has no call before it: it becomes text.
    let Some(results) = request.messages.get(2) else {
        panic!("{:?}", request.messages);
    };
    assert!(matches!(
        results.parts.as_slice(),
        [ir::Part::ToolResult(answered), ir::Part::Text(orphan)]
            if answered.call_id == "toolu_01A" && orphan.contains("toolu_01B")
    ));
}

#[test]
fn an_envelope_comes_back_as_its_provider_s_reasoning() {
    let sealed = envelope::seal("openrouter", &ir::Replay::Text);
    let request: wire::Request = serde_json::from_str(&format!(
        r#"{{"model":"m","max_tokens":10,"messages":[
            {{"role":"user","content":"hi"}},
            {{"role":"assistant","content":[{{"type":"thinking","thinking":"Two files.","signature":"{sealed}"}},{{"type":"text","text":"ok"}}]}},
            {{"role":"user","content":"go on"}}]}}"#
    ))
    .unwrap();
    let (request, _) = super::request(request).unwrap();
    let Some(assistant) = request.messages.get(1) else {
        panic!("{:?}", request.messages);
    };
    assert_eq!(
        assistant.parts.first(),
        Some(&ir::Part::Thinking(ir::Thinking {
            text: "Two files.".into(),
            replay: ir::Replay::Text,
            source: Some("openrouter".into()),
        }))
    );
}

#[test]
fn what_cannot_travel_is_refused() {
    for body in [
        r#"{"model":"m","max_tokens":1,"messages":[{"role":"user","content":[{"type":"image","source":{"type":"file","file_id":"f"}}]}]}"#,
        r#"{"model":"m","max_tokens":1,"system":[{"type":"image","source":{"type":"url","url":"u"}}],"messages":[]}"#,
    ] {
        let request: wire::Request = serde_json::from_str(body).unwrap();
        assert!(
            matches!(super::request(request), Err(TranslateError::Unsupported(_))),
            "{body}"
        );
    }
}

fn encode(events: Vec<ir::Event>) -> Vec<StreamEvent> {
    let mut encoder = StreamEncoder::new("qwen/qwen3-coder", "openrouter");
    let mut out = Vec::new();
    for event in events {
        encoder.push(event, &mut out);
    }
    encoder.finish(&mut out);
    out
}

#[test]
fn the_stream_opens_one_block_at_a_time_and_ends_once() {
    let out = encode(vec![
        ir::Event::Thinking("Two".into()),
        ir::Event::Thinking(" files.".into()),
        ir::Event::Text("Writing.".into()),
        ir::Event::ToolCallStart {
            id: "functions.Write:0".into(),
            name: "Write".into(),
        },
        ir::Event::ToolArgs(r#"{"file_path":"a"}"#.into()),
        ir::Event::ToolCallEnd,
        ir::Event::ToolCallStart {
            id: "call_2".into(),
            name: "List".into(),
        },
        ir::Event::ToolCallEnd,
        ir::Event::Usage(ir::Usage {
            input: 72,
            cache_read: 128,
            output: 50,
            ..ir::Usage::default()
        }),
        ir::Event::Stop(ir::StopReason::ToolUse),
    ]);
    let kinds: Vec<String> = out
        .iter()
        .map(|event| match event {
            StreamEvent::ContentBlockStart(start) => {
                format!("start {} {}", start.index, start.content_block.kind())
            }
            StreamEvent::ContentBlockDelta(delta) => {
                format!("delta {} {}", delta.index, delta.delta.kind())
            }
            StreamEvent::ContentBlockStop(stop) => format!("stop {}", stop.index),
            StreamEvent::MessageStart(_)
            | StreamEvent::MessageDelta(_)
            | StreamEvent::MessageStop(_)
            | StreamEvent::Ping(_)
            | StreamEvent::Error(_)
            | StreamEvent::Unknown { .. } => event.kind().to_owned(),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "message_start",
            "start 0 thinking",
            "delta 0 thinking_delta",
            "delta 0 thinking_delta",
            "delta 0 signature_delta",
            "stop 0",
            "start 1 text",
            "delta 1 text_delta",
            "stop 1",
            "start 2 tool_use",
            "delta 2 input_json_delta",
            "stop 2",
            "start 3 tool_use",
            "delta 3 input_json_delta",
            "stop 3",
            "message_delta",
            "message_stop",
        ]
    );

    let mut collector = Collector::default();
    for event in out {
        collector.push(event);
    }
    let message = collector.finish().unwrap();
    assert_eq!(message.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(message.usage.input_tokens, Some(72));
    assert_eq!(message.usage.cache_read_input_tokens, Some(128));
    let [
        ContentBlock::Thinking(thinking),
        _,
        ContentBlock::ToolUse(first),
        ContentBlock::ToolUse(second),
    ] = message.content.as_slice()
    else {
        panic!("{:?}", message.content);
    };
    assert_eq!(thinking.thinking, "Two files.");
    assert_eq!(
        envelope::open(&thinking.signature),
        Some(("openrouter".into(), ir::Replay::Text))
    );
    // An id Anthropic would refuse is mapped; a valid one is kept.
    assert!(first.id.starts_with("toolu_vsl_"));
    assert_eq!(first.input.as_text(), r#"{"file_path":"a"}"#);
    assert_eq!(second.id, "call_2");
    assert_eq!(second.input.as_text(), "{}");
}

#[test]
fn reasoning_with_only_an_opaque_form_is_still_a_block() {
    let out = encode(vec![
        ir::Event::ThinkingEnd(ir::Replay::ResponsesEncrypted("gAAAA".into())),
        ir::Event::Text("Hi.".into()),
        ir::Event::Stop(ir::StopReason::EndTurn),
    ]);
    let signatures: Vec<&str> = out
        .iter()
        .filter_map(|event| {
            if let StreamEvent::ContentBlockDelta(ContentBlockDelta {
                delta: Delta::Signature(signature),
                ..
            }) = event
            {
                Some(signature.signature.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(signatures, ["vsl1:openrouter:responses:gAAAA"]);
}

#[test]
fn a_stream_without_its_stop_ends_in_an_error() {
    let out = encode(vec![ir::Event::Text("Part".into())]);
    assert!(matches!(
        out.last(),
        Some(StreamEvent::Error(ErrorEvent { error })) if error.kind == ErrorKind::Overloaded
    ));
    assert!(
        !out.iter()
            .any(|event| matches!(event, StreamEvent::MessageStop(_)))
    );

    let failed = encode(vec![
        ir::Event::Text("Part".into()),
        ir::Event::Failed(ir::Failure::new(ir::FailureClass::RefusedRequest, "no")),
        ir::Event::Text("ignored".into()),
    ]);
    let errors = failed
        .iter()
        .filter(|event| matches!(event, StreamEvent::Error(_)))
        .count();
    assert_eq!(errors, 1);
    assert!(matches!(failed.last(), Some(StreamEvent::Error(_))));
}

#[test]
fn failures_before_the_answer_are_statuses_claude_code_acts_on() {
    let mut limited = ir::Failure::new(ir::FailureClass::RateLimited, "slow down");
    limited.retry_after = Some(Duration::from_secs(30));
    let response = error_response(&limited);
    assert_eq!(response.status, 529);
    assert_eq!(response.body.error.kind, ErrorKind::Overloaded);
    assert_eq!(response.retry_after, Some(Duration::from_secs(30)));

    let response = error_response(&ir::Failure::new(ir::FailureClass::Truncated, "cut"));
    assert_eq!(response.retry_after, Some(DEFAULT_RETRY_AFTER));

    let response = error_response(&ir::Failure::new(ir::FailureClass::ContextTooLong, "x"));
    assert_eq!(response.status, 400);
    assert!(
        response
            .body
            .error
            .message
            .starts_with("prompt is too long")
    );
    assert_eq!(response.retry_after, None);

    let response = error_response(&ir::Failure::new(ir::FailureClass::Unauthorized, "401"));
    assert_eq!(response.status, 403);
    assert!(
        serde_json::to_string(&response.body)
            .unwrap()
            .contains(r#""type":"error""#)
    );
}
