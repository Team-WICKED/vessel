//! The hand-written streams and requests under `tests/fixtures/`, for unit
//! tests (`tests/fixtures/README.md` says where they come from).

use crate::sse::{SseDecoder, SseEvent, SseFrame};

/// Anthropic: thinking, text, a tool call, usage, `message_stop`.
pub(crate) const ANTHROPIC_THINKING_TOOL_USE: &str =
    include_str!("../tests/fixtures/anthropic-thinking-tool-use.sse");
/// Anthropic: an `error` event after text began.
pub(crate) const ANTHROPIC_ERROR_MID_STREAM: &str =
    include_str!("../tests/fixtures/anthropic-error-mid-stream.sse");
/// Responses: reasoning, a message, a function call, `response.completed`.
pub(crate) const RESPONSES_REASONING_FUNCTION_CALL: &str =
    include_str!("../tests/fixtures/responses-reasoning-function-call.sse");
/// Chat Completions: two tool calls interleaved, usage, `[DONE]`.
pub(crate) const CHAT_PARALLEL_TOOL_CALLS: &str =
    include_str!("../tests/fixtures/chat-parallel-tool-calls.sse");

/// Every stream fixture, by name.
pub(crate) const STREAMS: [(&str, &str); 4] = [
    ("anthropic-thinking-tool-use", ANTHROPIC_THINKING_TOOL_USE),
    ("anthropic-error-mid-stream", ANTHROPIC_ERROR_MID_STREAM),
    (
        "responses-reasoning-function-call",
        RESPONSES_REASONING_FUNCTION_CALL,
    ),
    ("chat-parallel-tool-calls", CHAT_PARALLEL_TOOL_CALLS),
];

/// A Claude Code-shaped request.
pub(crate) const ANTHROPIC_REQUEST: &str = include_str!("../tests/fixtures/anthropic-request.json");
/// A Codex-shaped request.
pub(crate) const RESPONSES_REQUEST: &str = include_str!("../tests/fixtures/responses-request.json");
/// A Chat Completions request as the proxy would send it, and more.
pub(crate) const CHAT_REQUEST: &str = include_str!("../tests/fixtures/chat-request.json");

/// The events of a whole stream, comments left out.
pub(crate) fn events(stream: &str) -> Vec<SseEvent> {
    let mut decoder = SseDecoder::default();
    let mut frames = Vec::new();
    decoder.feed(stream.as_bytes(), &mut frames).unwrap();
    decoder.finish(&mut frames).unwrap();
    frames
        .into_iter()
        .filter_map(|frame| match frame {
            SseFrame::Event(event) => Some(event),
            SseFrame::Comment => None,
        })
        .collect()
}
