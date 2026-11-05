# Protocol

The types are defined in `crates/vessel-proto`; this file is the changelog
and a reader's summary. Bump `PROTOCOL_VERSION` on any breaking change and add
an entry here. Additive, optional fields do not need a bump.

## Version 2 (2026-11-05)

- Transport: one websocket at `/rpc`, JSON text frames.
  `Authorization: Bearer <token>` is required on the upgrade. `GET /health`
  stays open.
- Handshake: the client sends `{"type":"hello","protocol":{"min":2,"max":2},
  "client":{...}}`. The node replies with `welcome` (negotiated version plus
  `NodeInfo`), or with `rejected` and then closes.
- Calls: `{"type":"request","id":N,"call":{"method":"session.create",
  "params":{...}}}` is answered by `{"type":"response","id":N,"outcome":
  {"ok":{"method":"session.create","result":{...}}}}` or by
  `{"outcome":{"err":{"code":"not_found","message":"..."}}}`.
- Methods: `node.info`, `harness.list`, `session.list`, `session.get`,
  `session.create`, `session.send`, `session.kill`, `session.read`,
  `session.subscribe`, `session.unsubscribe`.
- Streams: `session.subscribe{session, from}` returns a subscription id. Then
  `{"type":"event","subscription":S,"record":{"seq":..,"at":..,"event":..}}`
  frames follow, ending with `subscription_ended`
  (`session_ended` | `unsubscribed` | `lagged{resume_from}`).
- Ordering guarantees (ADR 0006): the node never sends `event{S}` before the
  `response` that carries `Subscribed{S}`, and sends nothing for `S` after
  `subscription_ended{S}`. For `session.unsubscribe`, the client receives
  `subscription_ended{unsubscribed}` before the call's `ack`. Records within a
  subscription arrive in `seq` order with no gaps; a gap is only ever
  announced as `lagged{resume_from}`. If the node stops serving a
  subscription for any other reason after announcing it, it also ends it
  with `lagged{resume_from}` at the first record not written, so the client
  resyncs instead of waiting.
- Limits: each connection has a bound on calls in flight. Calls over the
  bound are answered at once with `unavailable`; the node keeps reading the
  connection, so `cancel` and `session.kill` always get through.
- Session modes: `terminal` (base64 PTY bytes) and `structured` (ACP-derived
  `AgentEvent` timeline).

## Version 1 (Phase 0)

REST routes and one websocket per session stream. Replaced entirely by
version 2.
