# Protocol

The types are defined in `crates/vessel-proto`; this file is the changelog
and a reader's summary. Bump `PROTOCOL_VERSION` on any breaking change and add
an entry here. Additive, optional fields do not need a bump.

## Version 5 (2026-11-06)

Nodes speak 2..=5; version 5 only adds, and older connections keep working
(below). Milestone 4 implements it (`docs/plans/backend-m4.md`); until its
tracks land, nodes answer the new methods with `unsupported`, record every
session as created by the owner, and never emit `profile_changed`.

- Every method has a first version (`since`, the table in `vessel-proto`
  `rpc.rs`): `node.stop` 3, `device.*` 4, the methods below 5, the rest 2.
  A call also needs a later version when it uses a later field
  (`Call::since`): `session.create` with `spec.profile`, `device.invite`
  with `scopes`. The client refuses such a call on an older connection
  without sending it (`CallError::NotInProtocol`, code `unsupported`), and
  the node refuses it (`unsupported`, "needs protocol N"), so neither side
  ever relies on the other ignoring a field it does not know.
- Profiles (ADR 0011): `SessionSpec.profile` (absent is the harness's
  default); `SessionInfo.profile` (the profile it runs with now) and
  `SessionInfo.created_by` (`{"kind":"owner"}`, `{"kind":"device","id"}`,
  `{"kind":"agent","node","session"}`; absent from older nodes, read as
  owner). `profile.list { harness? }` (read) and `profile.reload` (admin)
  return `ProfileInfo[]` (`name`, `label?`, `harness`, `sets`: variable
  names only, never values). `session.move_profile { session, profile?,
  force? }` (operate) returns the session. Event `profile_changed { from?,
  to? }`. Errors: `session.create` naming an unknown profile, or one of
  another harness, is `invalid_request`; `profile.reload` against a file
  that does not parse is `conflict` (the profiles in effect stay, and the
  message says where without quoting a value). `session.move_profile`:
  an unknown profile or one of another harness is `invalid_request`; a
  terminal session, or a harness whose session files Vessel cannot move,
  is `unsupported`; a running turn without `force`, a target profile that
  is not logged in, an ended session, a session that is still starting,
  missing or conflicting session files, and input sent while a move runs
  are `conflict`; an agent that does not start under the new profile is
  `spawn_failed` (the session goes on under its previous profile, with
  no `profile_changed`).
- Fleet (ADR 0012), scope `sync`: `fleet.manifest` returns the `Manifest`
  (`generation`, `files`: bundle path to `{hash, size, executable?}`,
  `mcp`: server name to transport, with `{"secret": "NAME"}` references,
  never values); `fleet.blob { hash, offset }` returns `{data (base64),
  total}`, at most 1 MiB per chunk; `fleet.watch { after }` is a long poll
  that returns a generation (newer, or `after` when the node's wait limit
  passes); `fleet.report { generation, conflicts? }` returns `ack`.
  `fleet.members` (admin) returns `FleetMember[]`.
- Bundle paths: `instructions.md` is the home's instructions file, which
  each member writes under its harness's own name (`CLAUDE.md`,
  `AGENTS.md`, `GEMINI.md`); `skills/<name>/...` is a skill, `<name>` being
  the `name` in its `SKILL.md` (not its directory name). Other top-level
  paths are reserved; a member ignores what it does not know. A path
  segment may not be a Windows device name (`CON`, `COM0`-`COM9`,
  `CONIN$`, ...) or a short-name alias (`NAME~1`). There is no mode beyond
  `executable`: members write fleet files owner-only.
- Scopes: `sync` is new. `device.invite` takes `scopes?`; without it a
  device gets `read`, `operate` and `admin`, as in version 4.
- Agents (ADR 0013): on the local socket, an agent's `vessel mcp` sends
  `{"type":"agent","session","secret"}` as its first frame instead of the
  token (a token is never a JSON object). Implemented with milestone 4
  Track H.
- Older connections: a node adapts what it sends to the negotiated
  version, where it encodes frames. `profile_changed` reaches a version 2-4
  client as `agent { harness_log { line: "vessel: profile changed from A
  to B" } }` at the same `seq` (the log stays gap-free), in subscriptions
  and in `session.read` pages alike, and `device.list` leaves out scopes
  the client's version does not have.
- Vessel's own notices in a session's log are `agent { harness_log {
  line: "vessel: ..." } }` records, in terminal sessions too (for example
  right after `starting`, an MCP server left out because this node lacks
  a secret, naming the secret, never a value). A terminal client that
  shows only terminal output can ignore them; `vessel log` prints them.

## Version 4 (2026-11-06)

Nodes speak 2..=4; version 4 only adds. Milestone 3 (relay, secure channel,
pairing) implements it; until then nodes answer `device.*` with
`unsupported` and send no `public_key`.

- Methods: `device.invite { label?, ttl_secs }` returns
  `{ link, expires }`; `device.list` returns `DeviceInfo[]`;
  `device.revoke { device }` returns `ack` and closes that device's
  connections.
- Scopes: every method declares one (`read`, `operate`, `admin`; the table
  in `vessel-proto` `rpc.rs`). Calls from a paired device without it get
  `forbidden`. The node token and the local socket hold every scope.
- `NodeInfo.public_key`: `"x25519:<base64url>"`, absent when unknown.
- Secure channel (ADR 0009): `Noise_NK_25519_ChaChaPoly_SHA256`, prologue
  `vessel/secure/1`, empty handshake payloads (both messages 48 bytes).
  Each carrier message is one record of at most 65,535 bytes: one sealed
  chunk, `flags` (`FIRST = 1`, `FINAL = 2`, others refused), the frame's
  `u32` big-endian length in a `FIRST` chunk, then data. Every chunk but a
  frame's last is full; frames are UTF-8 and at most 16 MiB. Any failed
  record or broken rule closes the channel. Details in
  `docs/plans/backend-m3.md` ("Seams", `vessel-secure`), implemented by
  `vessel-secure`.
- Relay possession proof: the same NK handshake, relay as initiator to the
  registering key, prologue `vessel/relay-proof/1`.
- Preamble inside the channel, before `hello`: the client sends
  `{"type":"device_auth","device","secret"}` or
  `{"type":"pair_request","invite","device_name"}`; the node answers
  `authenticated`, `paired` (with the new credential and `NodeInfo`), or
  `refused` with `unknown_device` | `revoked` | `invalid_invitation` |
  `unavailable`, then closes.
- Pairing link: `vessel://pair#` + base64url(JSON `PairingOffer`): node id,
  key, relay and direct routes (`ws://` or `wss://`), the single-use
  invitation, and its expiry. The offer sits after `#` so it never reaches a
  web server.
- Relay control connection (`vessel_proto::relay`): node sends `register
  {routing_id, key, token}`, relay sends `prove {message}`, node answers
  `proof {message}`, relay sends `registered` or `rejected {reason}`, then
  `incoming {conn}` per waiting client. Routing ids and connection ids are
  32 lowercase hex characters. Relay close codes: `4404` no such node,
  `4408` not accepted in time, `4413` too large, `4429` rate limited.
- Node close code on a secure-channel websocket: `4401` the handshake failed
  (most likely the client pinned another key). Relays forward a node's close
  code to the client unchanged. Clients treat `4401` as final: re-pair.

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
