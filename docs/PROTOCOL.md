# Protocol

The types are defined in `crates/vessel-proto`; this file is the changelog
and a reader's summary. Bump `PROTOCOL_VERSION` on any breaking change and add
an entry here. Additive, optional fields do not need a bump.

## Version 3 (2026-11-06)

Nodes speak 2..=3. Version 3 only adds, so version 2 peers keep working.

- `node.stop { "when": "now" | "when_idle" }` returns `ack`. `now` is the
  signal shutdown: live sessions end `interrupted`. `when_idle` drains: new
  `session.create` calls get `unavailable`, live sessions continue, and the
  node exits when none is live. A client that negotiated 2 never sends it.
  The `ack` is written before the node closes the connection, `now`
  included. (Builds before milestone 2's node side answered `unsupported`.)
- `NodeInfo.state`: `serving` | `draining`. When absent (a version 2 node),
  read it as `serving`.
- `vesseld attach` exit codes (`ProxyExit`): `0` closed, `80` no node
  answered and starting one failed, `81` data directory or token unusable,
  `82` token refused. `127` comes from the remote shell and means "not
  installed". Codes are never renumbered.
- Byte-stream framing, for SSH stdio and the node's local socket: each frame
  is a big-endian `u32` length, then that many bytes of UTF-8 JSON (the
  same text as a websocket frame). The maximum is 16 MiB, checked before
  allocating. On the local socket only, the first frame carries the token
  (ADR 0007), then the usual `hello`. A wrong or missing token is answered
  with `rejected` (`unauthorized`), and the connection closes.

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
