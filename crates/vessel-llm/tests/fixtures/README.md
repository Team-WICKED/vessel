# Stream fixtures

Server-Sent Event streams as each model API sends them, one file per case.
They are written by hand from the providers' documented stream formats,
not recorded from real traffic: the tests never reach a provider, and no
fixture holds a key, an account id or a real signature (signatures and
encrypted reasoning are placeholders).

- `anthropic-thinking-tool-use.sse`: Anthropic Messages with a thinking
  block (its signature in a `signature_delta`), text, a tool call whose
  arguments arrive in pieces, a `ping`, and usage in `message_start` and
  `message_delta`.
- `anthropic-error-mid-stream.sse`: an `error` event after text began.
- `responses-reasoning-function-call.sse`: OpenAI Responses with a
  reasoning item (summary and encrypted content), a message and a function
  call whose arguments arrive in pieces, ending in `response.completed`.
- `chat-parallel-tool-calls.sse`: OpenAI-compatible Chat Completions as
  OpenRouter sends it: a keepalive comment, `reasoning_content`, two tool
  calls interleaved by index, the finish reason and usage in separate
  chunks, then `[DONE]`.
