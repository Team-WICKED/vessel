# Protocol

The types are defined in `crates/vessel-proto`; this file is the changelog
and a reader's summary. Bump `PROTOCOL_VERSION` on any breaking change and add
an entry here. Additive, optional fields do not need a bump.

## Version 6 (2026-11-06)

Nodes speak 2..=6; version 6 only adds. Milestone 4's F3, F4 and Track X
implement it (`docs/plans/backend-m4.md`, "Protocol 6 (accepted)"), all
built: the envelope, `fleet.grant`, `fleet.callback`, `fleet.forget`,
`fleet.peers`, `fleet.nodes`, `session.read` with `wait`, the transfers
and the secret rows. (Until each landed, its methods answered
`unsupported`, "not built on this node yet", after the scope check like
every method.)

- Scope `relay`: passing calls on to other fleet nodes. Independent of
  `sync`, and a member gets it only when the home's owner asks (`vessel
  fleet invite --relay`); `vessel fleet invite` grants `sync` alone, never
  `admin`. `device.invite` naming `relay` needs protocol 6 (`Call::since`),
  and `device.list` leaves `relay` out for version 2-5 clients.
- Envelope (Track X): `fleet.forward { node, call }` (read) asks the
  caller's own node to run `call` on fleet node `node`; `fleet.relay {
  node, call, origin }` (relay) is a member asking its home to pass it on,
  refused when `origin.node` is not the member's own node; `fleet.deliver
  { call, origin }` (read) is the home having a member run it, the last
  hop, accepted only from that member's home. All three return
  `Forwarded { reply }`, the inner call's reply; an inner call that fails
  fails the envelope with its error. `origin` is `{ node, by, scopes }`,
  `by` one of `{"kind":"owner"}`, `{"kind":"device","id"}`,
  `{"kind":"agent","session","depth"}`. The target runs the inner call
  with its delivering device's scopes intersected with `origin.scopes`,
  and applies ownership and the agent limits to the origin. Subscriptions
  and the envelope's own methods cannot be forwarded
  (`Method::forwardable`). A call needs protocol 6 on the connection it is
  sent on; the inner call's version is checked where it is delivered.
- Fleet: `fleet.grant { link }` (sync) hands the home a pairing link made
  on the member, so the home holds a credential there (a member's node
  sends it when it connects, with the scopes chosen at join, `read` and
  `operate` by default; the device the home's redemption creates is the
  only one the member takes `fleet.deliver` from, and leaving revokes it).
  A member grants once per join: if its owner revokes the home's device,
  the home stays cut off until the node joins again.
  The home keeps a record of every node id it bound to a key and a name,
  and of the node every member device granted for, revoked and replaced
  devices included, and answers `conflict` to a grant that would rebind a
  device, bind an id to another key or a key to another id, rename a
  node, name the home's own id, or use a name the home or another member
  holds (also while that member's device is revoked). The same id and key
  through another device is the node joined again, and replaces the
  older grant. An id goes to a new key, or a node to a new name, only
  after `fleet.forget { node }` (admin, not forwardable; `vessel fleet
  forget <node>` against the home) and then only for a member device
  paired after it: the call withdraws the invitations outstanding on the
  home, a pairing that redeemed one before and completes after is
  revoked, and it returns `NodeForgotten { key, name?, devices,
  withdrawn }`. Any member device paired after it may then claim the id
  and name, and the first claim binds them, so the owner invites the node
  again before anything else and it joins from there. A
  refusal over a revoked or forgotten node says only that the name is
  reserved for, or the key bound to, another node, without ids or times;
  one over a live member's name names that member; the home's log has the
  details. `conflict` also while the same device has a grant in
  progress; `invalid_request` to a link with no route or more than 8; and
  `unavailable`, without details, when it cannot pair (within 30 s).
  `fleet.callback { node, key }` (sync; from a member device only, never
  forwarded) returns `Callback { relays, ticket? }`: the relays the home
  registers with and a registration ticket for `key` (`RelayTicket { key,
  expires_at, mac }`, never the relays' token; none when the home has no
  relay). A device the home's record binds to `node` and `key`, holding a
  current grant, gets a ticket for a day. One that has not granted yet gets
  one for ten minutes, only within an hour of its pairing (never reaching
  past that), only for a claim the record would take, and for one node and
  key, which the home records and its grant must then claim (`conflict`
  otherwise); others get `forbidden`. A member whose node has no route of
  its own asks on each connection, and again halfway to the ticket's
  expiry while connected, registers with those of the relays it dials (at
  most 8: any on a host it reaches its home by; otherwise `wss://`, not
  `localhost` nor a non-canonical numeric host, and only at public
  addresses its host resolves to when dialed) and names them, and only
  them, in its grant's link; another home's callback, or leaving, unlinks
  them. With none, it sends no grant and records why
  in its fleet state. `fleet.peers` (sync) and `fleet.nodes` (read)
  return `FleetNode[]` (`node`, `name`, `protocol`, `reachable`), the
  home included.
- Transfers (F4): `session.export { session, force? }` (operate,
  ownership-checked for agents) and `memory.export { profile?, project? }`
  (admin) snapshot files into a transfer and return `TransferManifest {
  transfer, files, detail }`, `files` in the bundle's shape (path to
  `{hash, size, executable?}`), `detail` `{"kind":"session",
  "harness_session", "cwd", "info"}` or `{"kind":"memory", "harness",
  "profile"?, "project"?}`. `transfer.read { transfer, hash, offset }`
  (read) returns a `BlobChunk` as `fleet.blob` does; `transfer.close {
  transfer }` (read) drops it; only the caller that opened a transfer may
  read or close it, and transfers expire after 10 idle minutes and at
  restart. `session.import { from: {node, session}, profile?, cwd?, start?
  }` (operate) pulls a session here and returns `SessionImported {
  harness_session, profile?, cwd, written, session? }`; `memory.import {
  from: {node, profile?, project?}, profile?, project? }` (admin) returns
  `MemoryImported { written, same, beside? }`, conflicting files written
  beside the existing ones. Limits: 256 MiB per file and 2 GiB per
  transfer (`MAX_TRANSFER_FILE`, `MAX_TRANSFER_TOTAL`); a transfer over
  either is `invalid_request`, naming the limit and the size.
  As built (F4): an import from another fleet node sends the source's
  `session.export` or `memory.export`, `transfer.read` and
  `transfer.close` through the envelope as the importing caller, so the
  source's grant to the home must cover them (`admin` for memory) and its
  transfer belongs to that delivery; a node in no fleet answers
  `not_found`. A profile with no harness home on the target yet is
  `conflict`, before anything is fetched. A `TransferId` is a UUIDv4;
  the holder check, not the id, keeps a transfer to its opener (another
  caller gets `not_found`, as for an unknown id). At most 16 transfers
  are open on a node (`unavailable` beyond). `session.export` of a
  terminal session is `unsupported`, of one whose agent is still starting
  `conflict`; `force` copies the transcript as far as it is written,
  cancelling nothing. `memory.export` without `profile` is Claude Code's
  default profile. `session.import` refuses (`conflict`) the profile and
  directory the session already is in on the same node, and a different
  file in the way, before writing anything; an unknown target profile is
  `invalid_request` before the source is asked; `start` needs the
  directory to exist (`invalid_request`). `memory.import` with `project`
  needs `from.project`.
- Secrets (F3), all admin: `secret.set { name, value }`, `secret.list`
  (names only), `secret.delete { name }`, `secret.copy { name, to }`. There
  is no `secret.get`: a value leaves a node only into another node's store.
  `secret.copy` sends `secret.set` to `to` through the envelope, so it
  needs the target's owner to have granted the home `admin` there.
  As built: `secret.copy` of a secret not set here is `not_found`, to this
  node itself `invalid_request`; a target that did not grant the home
  `admin` is `forbidden`, the message saying how to (`vessel fleet join
  --grant read,operate,admin` there), and the home itself, which grants
  its members only `sync` and `relay`, is `forbidden` with a pointer to
  `vessel fleet secret set` there. No error carries a value. The target
  is asked `secret.list` through the envelope first (forwardable for
  this, names only), and `secret.set` carrying the value goes only if that
  passes, so a target that would refuse the value never receives it.
- `session.read` takes `wait?` (seconds, capped at 60, protocol 6): with
  nothing at `from` yet, the node answers as soon as a record lands there
  or the session ends, or when the wait ends (an empty page). An ended or
  stored session answers at once. Waiting reads may hold at most half of a
  connection's calls (32 of 64); one origin at most 8 on a node; all
  origins of one other node at most 16 there; and on the home, the reads
  one relaying member device passes on at most 16, whatever origins they
  name. A read past any of these answers at once, as without `wait` (passed on to
  another node, it goes without its wait). A client that gets an empty
  page before its wait could have ended should pause before asking again.
- `SessionInfo` takes `via?` (the home a creating call came through) and
  `origin?`: `{"kind":"remote","node","by"}` (`by` owner or `{"kind":
  "device","id"}`) for a session created by a forwarded call, `{"kind":
  "imported","node","session"}` for an imported one. Absent for a session
  of this node's own, so older clients see nothing new; `created_by` uses
  only the variants protocol 5 has.

- Relay registration and limits (the relay's own wire, not negotiated;
  2026-11-06):
  - `register` may carry `ticket {key, expires_at, mac}` instead of
    `token` (exactly one of the two, else `rejected {reason: "token"}`):
    HMAC-SHA256 under the relay's token over `"vessel relay ticket
    v1\0"`, the key's 32 bytes and `expires_at` (seconds since the Unix
    epoch, 8 bytes big-endian). A fleet's home mints one for a member's
    key (`fleet.callback`). The relay takes it only for the key that then
    proves possession, unexpired and expiring at most 48 h ahead
    (`rejected {reason: "ticket"}` otherwise), and closes a registration
    made with it when it expires; the node registers again with a newer
    one.
  - A relay holding its most nodes answers a registration `rejected
    {reason: "full"}` (after the token or ticket and the proof); a node
    registered with a ticket holds a place like any other and frees it
    when the ticket expires. It closes a client or a still-registering
    control connection past its connection limits with `4503` (full,
    transient), and answers a socket past its limits at accept `503`
    before closing it, including one past its address's rate of new
    connections; through a trusted reverse proxy, where the client's
    address is known only from the request, that rate is checked at the
    upgrade and refused with `4429`. A connection that authenticates (a
    node registered, a node's data connection claimed, a client still
    spliced 7 s after pairing, which its node has admitted) gives its
    share of the rate back. An IPv6 peer is counted by its `/64`, `/56`
    and `/48` (the wider blocks with two and four times each per-address
    limit). A client of one node from one address past
    `--max-node-clients-per-address` while the node admits them is closed
    with `4503`. Behind a trusted proxy, a client-address header that is
    not one bare address is answered `400`.
  - Deadlines a peer must keep: a request's headers within 10 s; a
    node's first control message within 2 s of the upgrade; a client's
    first record (its handshake message) within 2 s of the relay pairing
    it with the node. When every open registering place is taken, a
    control connection holds none until its first message: a key
    registered at that relay before takes one of the places kept for
    known nodes, if one is free; any other is refused `full` at once.
  - A node gives a channel a relay spliced to it 5 s
    (`vessel_relay::RELAYED_ADMISSION`) for the Noise handshake and the
    device preamble together, and drops it otherwise (a direct channel
    keeps 10 s for each); at most 40 channels wait at once through each
    relay link, apart from 64 direct ones, of which one address (an IPv4
    address or an IPv6 `/64`) holds 7, a `/56` 14 and a `/48` 28; past
    those the carrier is dropped at once. Behind a trusted reverse proxy
    (`vesseld --trusted-proxy`) the address is the client's from the
    proxy's header, in the one form a relay takes (one bare address, else
    `400`; without it, the proxy's own). A client must send its preamble
    as soon as its handshake is done.
  - A node that does not know `full` or `ticket` reads the message as
    malformed and retries; a client that does not know `4503` sees a
    plain close; a relay from before tickets refuses a ticket as
    malformed. The limits themselves are the relay's configuration
    (`vesseld relay --max-*`, README).

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
  that returns the home's generation as soon as it differs from `after`
  (older too, so a member follows a home whose data was reset), or `after`
  itself when the home's wait limit (50 s) passes; `fleet.report { generation, conflicts? }` returns `ack`.
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
  token (a token is never a JSON object). A credential that is unknown,
  wrong, or of an ended session is answered like a wrong token
  (`rejected`, `unauthorized`); an admitted connection closes when its
  session ends. An agent has its session creator's scopes, at most `read`
  and `operate`; `session.send`, `session.kill` and `session.move_profile`
  on a session it did not start (directly or through sessions it started)
  are `forbidden`, as are a ninth live session of its own and a session
  four levels of agents deep. Sessions it creates record `created_by:
  {"kind":"agent","node","session"}`.
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
  `incoming {conn}` per waiting client (tickets and the `full` refusal
  came with version 6: "Relay registration" there). Routing ids and connection ids are
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
