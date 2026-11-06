# ADR 0009: A dumb relay, and one end-to-end secure channel

Status: accepted, 2026-11-06, with the coordinator decisions in
`docs/plans/backend-m3.md`; built in milestone 3.

## Context

Milestone 2 reaches a node only where `ssh` reaches it. Many machines
cannot be reached that way: a laptop behind NAT, a desktop with no
`sshd`, a phone (a later client). ORIGIN item 2 asks for remote use that is
"very convenient", and VISION section 2 promised a relay so that machines
reach each other without a VPN.

The draft roadmap said to build the relay on an existing overlay
(Tailscale's `tsnet`, or raw WireGuard). On 2026-11-06 the owner chose
Paseo's shape instead, after asking how Paseo's relay and pairing work:
a dumb relay, embedded in our own binary, with encryption end to end.

What exists today:

- Every protocol frame is JSON text over a `FrameTransport` (a
  `Stream<String> + Sink<String>`). A websocket and the length-prefixed byte
  stream of protocol 3 both carry it, so a new transport plugs in under
  `NodeClient::over` without touching the RPC layer.
- ADR 0005: every connection presents the node token. That is right for
  the machine's own clients, and wrong for a relay: a token is one shared
  secret per node, and it cannot be revoked for one device.
- ADR 0005's amendment: no TLS stack is built yet. TLS was to be chosen
  "together with the relay".
- ADR 0007: reconnecting needs no replay buffer. Calls in flight at a drop
  fail with `Disconnected` and are never retried; reads and subscriptions
  resume from their `EventSeq` cursor (`follow_resuming`).
- `docs/LESSONS.md`: Paseo kept two crypto modes alive for compatibility.
  Vessel has one transport security story per phase.

What Paseo does (read 2026-11-06, ideas only; details and file references
in `docs/plans/backend-m3.md`, "Grounding"):

- The daemon keeps a control websocket to the relay and opens one data
  websocket per client. The relay forwards frames unchanged.
- Client and daemon run `Noise_NK_25519_ChaChaPoly_SHA256` through the
  relay, keyed by the daemon's public key from the pairing offer. Records
  use counter nonces and are chunked for Noise's 64 KiB limit.
- Paseo also keeps a legacy NaCl-box mode alive beside Noise. Two
  ephemerals per handshake, worst-case size budgets, downgrade guards, and
  replay stays possible while legacy clients are admitted.
- Its relay has no authentication and no rate limits. Anyone who knows a
  daemon's id can replace its control socket: a denial of service E2EE
  cannot prevent.
- It buffers up to 200 early client frames and drops the oldest. It
  replaces the daemon's close codes, so clients cannot see why the daemon
  refused them.

## Options

### Where the relay comes from

1. **An overlay network** (`tsnet`, WireGuard, a hosted mesh). It solves
   NAT traversal, but it needs a coordination service or an account (or
   hand-managed WireGuard peers), and a second daemon to configure and
   update. If its keys are the only protection, the overlay is part of the
   trust base.
2. **A hosted rendezvous** run by a third party. Simple for users, but it is
   a service to depend on or to run, and to trust.
3. **Our own dumb relay, built into `vesseld`.** `vesseld relay --bind ...`
   on any machine with a public address. It pairs connections by node and
   forwards bytes. It is not trusted with anything but availability.

### Transport security

1. **TLS to the node** (a certificate per node, pinned). Needs a TLS stack
   on both ends and certificate handling for nodes without DNS names. It
   protects only up to whoever terminates TLS, which with a relay is the
   relay.
2. **Noise, one pattern, end to end** inside whatever carries the bytes
   (relay or direct). The node has a static key; the client knows it from
   pairing (Noise NK). Two messages, then authenticated encryption.
3. **Both**: Noise end to end, and TLS on the hop to the relay. TLS adds
   nothing to confidentiality or integrity here, but it hides routing ids
   from the network and lets the relay sit behind an existing HTTPS
   reverse proxy on port 443.

### Implementation of the Noise channel

1. **`snow`** (Noise for Rust), on RustCrypto primitives.
2. **Hand-rolled** Noise NK on the same RustCrypto primitives
   (`x25519-dalek`, `chacha20poly1305`, `sha2`).
3. **`noise-protocol` + `noise-rust-crypto`**, a smaller generic crate pair.

## Decision

### The relay

Option 3: our own relay, in the same binary. `vesseld relay --bind ADDR`
runs it; it has no data directory and stores nothing.

- **Roles.** A node keeps one *control* connection open to each of its
  relays, registered under its routing id (below). A client connects to the
  relay naming a routing id. The relay tells the node over its control
  connection, and the node opens a *data* connection for that client. The
  relay then splices the client's connection and the node's data connection
  and forwards their binary messages unchanged.
  - One data connection per client, not a multiplexed control connection:
    each pair keeps its own TCP backpressure, and the relay needs no stream
    ids, windows or buffers of its own.
- **Routing id.** The relay addresses a node by a routing id derived from
  its static public key: the first 16 bytes of `SHA-256(node public key)`,
  as lowercase hex (base32 in the draft; the protocol 4 contract chose
  hex). Not by `NodeId`, so the relay can check that a node registering an
  id holds its key.
  - On registration the relay runs a Noise NK handshake as initiator
    towards the claimed key, and accepts the registration only if the node
    completes it. Only the holder of the private key can, so nobody can
    take over another node's slot and black-hole its clients.
  - That handshake has its own prologue, `vessel/relay-proof/1`, so a
    relay's challenge is no client handshake for the node's `/secure`
    endpoint, and the node's answer is no channel handshake.
- **What the relay sees.** Routing ids, IP addresses, timing and message
  sizes. Never a plaintext byte, a session id, a token or a credential. A
  malicious relay can drop, delay, reorder or replay messages, and every one
  of those either closes the channel (below) or is a plain denial of
  service.
- **No buffering.** Until a client is paired with the node's data
  connection, the relay reads nothing from it. The client's first bytes wait
  in the socket, with TCP backpressure, and nothing is held or dropped by the
  relay (Paseo buffers 200 frames and drops the oldest).
- **Limits.**
  - Every message's size is checked against one record (65,535 bytes, the
    Noise limit) before it is read.
  - At most 64 client connections may wait for one node, each for at most
    10 s for the node's data connection.
  - Idle connections are pinged, and a connection silent for 30 s is
    dropped, so half-open sockets do not linger.
  - When either side of a pair goes, the relay closes the pair.
- **Reasons travel end to end, not as close codes.** A node that refuses a
  client (revoked device, used invitation) says so in an encrypted frame
  before closing. Relays and proxies rewrite websocket close codes (Paseo
  hit this), so nothing depends on them.
- **Who may use a relay.** Registering a node needs the relay's
  registration token (a secret given to the nodes that may use it), so
  strangers cannot use our relay as free infrastructure. Connecting as a
  client needs nothing: a client is useless without the node's key and a
  device credential. Client connections are rate limited per address.

### Transport security

Option 2, Noise end to end, one pattern, no other mode:
`Noise_NK_25519_ChaChaPoly_SHA256`. SHA-256 rather than BLAKE2s: the
published test vectors (`cacophony`, `noise-c`) cover this exact suite, and
`sha2` is already in the workspace.

- **Keys.** The node creates a static X25519 key on first start,
  `<data_dir>/node.key` (0600, like `token`). Its public half is shown by
  `vessel node status` and carried by pairing links (ADR 0010). It is the
  node's identity on the wire; `NodeId` stays the name in the book and the
  protocol.
- **Prologue** `vessel/secure/1`, so a handshake for anything else cannot
  be fed to ours. The version is the only negotiation: a peer that speaks a
  different one fails the handshake. A future change is a new label and a
  new protocol version, never a second mode kept alive (`LESSONS.md`).
- **Records.** After the handshake, each binary message is one record: the
  ChaCha20-Poly1305 ciphertext of one chunk, at most 65,535 bytes (the Noise
  limit).
  - The nonce is a 64-bit counter per direction, implicit, as Noise
    transport mode defines it. Any replayed, reordered, dropped or forged
    record fails authentication, and the channel closes. The carrier (a
    websocket over TCP) is reliable and ordered, so a failure means
    tampering or a bug, never ordinary loss.
  - A chunk's plaintext starts with a flags byte (`FIRST`, `FINAL`; other
    bits are refused). A `FIRST` chunk then carries the whole frame's length
    (`u32`, big-endian). One protocol frame (JSON, up to the protocol's
    16 MiB) is one or more chunks.
  - The receiver checks each record's length before reading it, and the
    announced frame length against 16 MiB before buffering anything. Each
    chunk must fit within the announced length, and `FINAL` must come exactly
    at its end. A peer therefore cannot make the receiver buffer more than
    one frame, nor send a frame that never ends.
  - No rekeying: 2^64 records per direction is never reached. The counter
    stops at its maximum, and the channel closes.
- **Above the channel**, the client authenticates itself in the first
  frame: its device credential (ADR 0010), or a pairing invitation. Then
  `hello`/`welcome` as on every transport.
- **Carriers.** The same channel runs:
  - over the relay (websocket binary messages);
  - directly, on the node's existing listener at a new path `/secure`, for a
    node with a reachable address. No token: the device credential inside
    the channel replaces it.
  - The node's `/rpc` (bearer token, ADR 0005) and local socket stay as they
    are, for the machine's own clients and SSH.
- **TLS** (option 3) only on the hop to the relay, and only as an option:
  `wss://` relay URLs work when the relay sits behind an HTTPS reverse proxy
  (the realistic deployment on an existing public machine). This needs a
  TLS client in `vessel-client` and `vessel-node` with an explicit crypto
  provider (plan, open decision 1). The relay itself speaks plain HTTP.

### Implementation

Option 1, `snow`, behind our own crate `vessel-secure`.

- **Why `snow`.**
  - It is the established Noise implementation for Rust, written so that the
    state machine cannot be misused: the handshake order is enforced, no
    transport message is possible before the handshake, and the nonce cannot
    be reused. Its own source has no `unsafe`.
  - It carries the spec's test vectors, which we would otherwise have to
    port and maintain.
  - A hand-rolled NK is about 300 lines, but those lines are exactly where
    subtle mistakes live: a missing `MixHash`, a wrong `Split` order, nonce
    handling. We would own them with no reviewer outside this repository.
- **What it pulls in** (checked 2026-11-06, `snow` 0.10.0 with
  `default-features = false` and `use-chacha20poly1305`, `use-sha2`,
  `use-curve25519`, `use-getrandom`; the build was run with `use-blake2`,
  whose tree differs only by `blake2`):
  - about 33 crates, most already in the workspace through other crates
    (`getrandom` 0.3, `subtle`, `zeroize`, `cfg-if`, `libc`, `syn`);
  - the cryptography is RustCrypto: `chacha20poly1305` 0.10 (audited by NCC
    Group, 2020) and `curve25519-dalek` 4 (audited by Quarkslab, 2019).
  - Without `std`, so no `ring`: `snow`'s `std` feature enables its optional
    `ring` dependency (C and assembly). An NK round trip was built and run
    without it.
- **What it does not give us.**
  - `snow` itself has had no formal audit (its README says so). The
    primitives have.
  - We mitigate in three ways:
    - interoperability tests against the published Noise test vectors for
      our exact suite (the `cacophony` and `noise-c` NK vectors Paseo also
      checks against);
    - a fuzz-style test of the record parser;
    - keeping `snow` behind `vessel-secure`, so it can be replaced without
      touching the rest.
- **Option 3** (`noise-protocol`) is smaller but less used and less
  maintained, with no audit either. It brings no advantage over `snow`.

## Why

- **No third-party account or service.** A relay is one more process of the
  binary users already have, on any machine with a public address.
- **The relay is out of the trust base.** With Noise end to end and keys
  pinned at pairing, a compromised relay (or the network under it) learns
  metadata and can deny service, nothing more. An overlay would not add
  security on top of that, only another thing to run.
- **One mode.** One Noise pattern, one suite, one record format, and a
  prologue that changes with the version. Paseo's experience with two
  modes kept alive is the cautionary tale.
- **Direct and relayed are the same channel**, so there is one security
  story to review and test, and the node book only chooses the route.
- **Reconnect stays simple.** The relay buffers nothing and promises
  nothing. A dropped relay connection is exactly a dropped SSH connection
  for ADR 0007's rules.

## Consequences

- New crates `vessel-secure` (Noise channel and records) and `vessel-relay`
  (the relay server and the node and client sides of its protocol). Edges
  are in the plan and, once accepted, in ADR 0001.
- A new key file in the data dir, and a migration that creates it for
  existing nodes on start.
- Protocol 4 documents the secure channel, the relay protocol and the
  device frames in `PROTOCOL.md`.
- Running a relay is an operator task: a public machine, the registration
  token, optionally a reverse proxy for `wss://`. Nothing about it needs
  persistence or backups.
- Losing `node.key` means re-pairing every device. Rotating it is an
  explicit later operation; M3 has none.
- Metadata (who talks to which routing id, when, how much) is visible to
  the relay operator. That is accepted: the operator is the owner.

## Amendment: registration tickets (protocol 6, 2026-11-06)

A fleet member with no route of its own registers with its home's relay,
so the home can call it back (`fleet.callback`, milestone 4). Handing it
the registration token let any device that asked register any key, for
good (the review of milestone 4, round 6), so a member gets a ticket
instead: one node key and an expiry, with HMAC-SHA256 over both keyed by
the relay's token (`vessel_relay::issue_ticket`, `check_ticket`). The
relay accepts the token or a ticket, a ticket only for the key the node
then proves it holds, unexpired and at most 48 h ahead, and closes a
registration made with one when it expires. The relay keeps no new state
or configuration; rotating its token voids every ticket. Whoever holds the
token mints tickets: the operator, and a fleet's home that registers with
the relay, which gives them only to its members' devices for their own
keys (`docs/plans/backend-m4.md`, review round 6).
