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

Requests, as JSON bodies:

- `anthropic-request.json`: shaped like Claude Code's: a system prompt in
  blocks with a cache breakpoint, thinking with a budget, a client tool
  with and without its `type` and a server tool, a `role: "system"`
  message, an image by base64 and by URL, a text document, thinking and
  redacted thinking, a tool call and its results (one an error), a
  `server_tool_use` block and a top-level field Vessel does not model.
- `responses-request.json`: shaped like Codex's: instructions, developer
  and user messages (one without its `type`), encrypted reasoning, a
  function call and a free-form (`apply_patch`) call with their outputs,
  an item kind Vessel does not know, function, grammar and hosted tools,
  and Codex's reasoning, `include` and cache-key settings.
- `chat-request.json`: a Chat Completions request as the proxy writes one,
  plus a few forms servers accept (`stop` as one string, a message with
  the old `function` role).
