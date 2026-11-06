# Backend milestone 3: any node, with no SSH and no open port

Status: proposed (draft 2026-11-06). Starts once milestone 2 is merged and
green on all three CI platforms (it is: `backend/m2` at 2cd316b). Owner:
coordinator. Two implementation tracks, as in M1 and M2.

Read first: `AGENTS.md`, `docs/VISION.md` section 2 (the change of
direction), `docs/LESSONS.md` (the Paseo rows, above all "two crypto modes
kept alive"), ADRs 0005 and 0007, and the two proposed ADRs this plan
depends on: 0009 (the relay and transport security) and 0010 (pairing and
device principals). "Grounding" at the end summarises what Paseo does, with
file references.

## State (2026-11-06)

- **Track P, on `backend/m3-pair`:**
  - store: `node.key` (32 bytes, 0600, created on every start, never
    replaced); the `devices` table (schema version 2) with SHA-256
    credential hashes compared in constant time, revoked devices kept as
    tombstones.
  - node: principals per connection and the scope check for every call
    (`guard::Guarded`); `device.invite` (in-memory, single-use, by hash,
    at most a day and 64 waiting), `device.list`, `device.revoke` (closes
    the device's connections); the device preamble and `SecureGateway`
    over any frame transport; `--relay`, `--relay-token-file`, `--public`.
  - client: paired nodes in the book (`key`, `relay`, `direct`,
    `credential`), credential files, `pairing::pair`/`authenticate`, and
    `NodeClient` device calls.
  - CLI: `vessel node pair` (link and QR code), `vessel node devices
    [revoke]`, the key fingerprint in `node status` and `nodes show`.
  - With `vessel-secure` (936dd9d): the node's key pair from `node.key`,
    `NodeInfo.public_key`, real pairing links, `SecureGateway::accept`
    (respond, preamble, protocol); the client's `secure::open`/`pair` over
    any carrier. Tested over `memory_pair` against a real node.
- **Waiting for `vessel-relay`:** `AxumCarrier` for `/secure` (closing
  with 4401 on a failed handshake) and relay data connections into
  `SecureGateway::accept`; `SecureConnector` (route order, the websocket
  dial, 4401 as final) and the `wss://` TLS client; `vessel nodes add
  --pair` and connecting to paired nodes; resume over the relay (P5).

## Goal

Roadmap Phase 1, the relay:

- A node behind NAT, with no `sshd` reachable and no open port, is used from
  a laptop as if it were local:

  ```bash
  # on the node's machine (or over SSH, or from a paired admin device)
  vessel node pair --label laptop          # prints a link (and a QR code)
  # on the laptop
  vessel nodes add gpu-box --pair 'vessel://pair#eyJ2IjoxLC...'
  vessel --node gpu-box run claude
  ```

- The node keeps an outbound connection to a relay that we run ourselves:
  `vesseld relay --bind 0.0.0.0:7443`, the same binary, on any machine with
  a public address. The relay pairs a client's connection with the node's
  and forwards bytes it cannot read (ADR 0009).
- Every remote connection, relayed or direct, is one Noise NK channel to the
  node's static key, which the client pinned at pairing. There is one mode
  and no fallback.
- A node with a reachable address is dialled directly over the same
  channel, and the relay is the fallback. The node book chooses.
- Every device has its own credential, made by an expiring, single-use
  invitation, listed by `vessel node devices`, and revocable without
  touching the others (ADR 0010).
- A dropped relay connection behaves like a dropped SSH connection
  (ADR 0007): attached clients resume from their cursor, and calls in flight
  report that their outcome is unknown. Nothing is retried.

## Out of scope

- **Node-to-node peers and routed calls** (Paseo's connected hosts, the
  fleet's "every agent sees every agent"). They need agent principals and
  grant intersection, and come after M3, on top of its channel and
  principals.
- **Semantic permissions narrower than "owner"** (ADR 0010). M3 classifies
  every method by scope and grants all scopes; narrower invitations are an
  additive later change.
- **Short typeable pairing codes** (a PAKE such as SPAKE2). The link and its
  QR code only (open decision 3).
- **NAT hole punching and peer-to-peer UDP.** Direct means the node's
  address is reachable as it is; everything else goes through the relay.
- **A hosted relay**, or any relay URL built into `vessel`. The owner runs
  the relay (open decision 5).
- **Node key rotation.** Losing or replacing `node.key` means pairing every
  device again (ADR 0009, consequences).
- **Mobile and web clients.** The link format and QR code are made ready
  for them; the clients themselves are Phase 5.
- **Latency probing between routes** (Paseo probes every route and switches
  to the fastest). M3 tries routes in a fixed order (open decision 6).

## What M2 gives us

- `Connector`, `Pinned` and `follow_resuming` (vessel-client): a transport
  is one more `Connector`. Pinning, backoff, resuming from the cursor and
  "never retry a call" already hold for any transport.
- `FrameTransport`, any `Stream<String> + Sink<String>`, carries the
  protocol, so the secure channel only has to be such a stream.
- The node book, layered config, and ids pinned on first contact. M3 adds
  route and key fields, and credentials kept outside the book.
- The node's listener, the local socket, `vesseld attach`, and `node.stop`.
  M3 adds a path and a background task; it removes nothing.
- `DataDir` (0700, files 0600), SQLite migrations (`user_version`).

## Contract changes (coordinator, before the tracks start)

`vessel-proto` stays frozen for the tracks, as before.

1. **Protocol 4.** `PROTOCOL_VERSION` becomes 4; nodes speak 2..=4, and
   every change is additive.
   - `device.invite { label, ttl_secs } -> Invitation { link, expires }`
   - `device.list -> Vec<DeviceInfo>`; `DeviceInfo { id, label,
     created_at, last_seen, scopes, revoked_at }`
   - `device.revoke { device } -> Ack`. It also closes that device's open
     connections.
   - `NodeInfo.public_key: Option<NodePublicKey>`. Absent from older nodes;
     shown by `vessel node status`, and used to make a pairing link over an
     existing connection.
   - `ErrorCode::Forbidden`, for a principal without the method's scope. M3
     grants every scope, so only tests see it.
2. **Scopes** (ADR 0010).
   - `Scope { Read, Operate, Admin }`.
   - The `rpc_methods!` table gets a scope column, so every method,
     `node.stop` and `device.*` included, declares one.
   - The node checks it for every call from a device. The token and the
     local socket hold every scope.
3. **Secure channel preamble** (`vessel_proto::device`). These are the first
   frames inside a secure channel, before `hello`:
   - client: `DeviceAuth { device, secret }` or `PairRequest { invite,
     device_name }`;
   - node: `Authenticated { device }`, `Paired { device, secret, node:
     NodeInfo }`, or `Refused { reason: RefusalReason }`, where the reason is
     one of `UnknownDevice`, `Revoked`, `InvalidInvitation` or `Unavailable`.
4. **Relay protocol** (`vessel_proto::relay`): the relay's own JSON control
   messages and its error close codes. They live in `vessel-proto` because
   wire types exist once (`ARCHITECTURE.md`, design rules). The details are
   under "Seams".
5. **`PROTOCOL.md` version 4** documents all of it:
   - the secure channel: suite, prologue, handshake payloads, record and
     chunk format, limits, and how the channel closes;
   - the relay protocol;
   - the pairing link format.

## Crates (ADR 0001)

| Crate | Change | New edges |
| --- | --- | --- |
| `vessel-secure` (new) | Noise NK through `snow` (ADR 0009). Keys (`NodeKeypair`, `NodePublicKey`, `RoutingId`, fingerprints). The record layer (chunking, size checks). `SecureChannel<C>`, which turns any binary-message carrier into a `FrameTransport` | **new crate: `-> proto`**, for the key types (see "Contract as built"); it knows nothing of the protocol's frames: it moves UTF-8 strings |
| `vessel-relay` (new) | The relay server (`serve`), the node's link to a relay (registration, possession proof, data connections), and the client's dial | **new crate: `-> proto, secure`** |
| `vessel-store` | `node.key` (32 random bytes, 0600, created on first start). The `devices` table (migration). Credential hashes | none (a key is 32 random bytes; deriving its public half is `vessel-secure`'s job) |
| `vessel-node` | `/secure` on the existing listener; the device preamble; scope checks; `device.*`; one relay link per configured relay; `vesseld relay` subcommand; `NodeInfo.public_key` | **`-> secure, relay`** |
| `vessel-client` | `SecureConnector` (direct, then each relay), `wss://` through a TLS client (open decision 1), book fields (`key`, `relay`, `direct`, `credential`), pairing | **`-> secure, relay`** |
| `vessel-cli` | `vessel node pair`, `vessel nodes add --pair`, `vessel node devices [revoke]`, QR code (open decision 2) | none |
| `vessel-testkit` | A relay that misbehaves on command: drop, reorder, replay, corrupt, delay (built: feature `relay`, a websocket proxy in front of a real relay, `vessel_testkit::relay::MisbehavingRelay`) | none (dev-only) |

ADR 0001 and `ARCHITECTURE.md` gain these edges in the same change that adds
the crates.

## Seams

### `vessel-secure`

As built (Track S, 2026-11-06; `crates/vessel-secure`, the source of
truth):

```rust
pub const SUITE: &str = "Noise_NK_25519_ChaChaPoly_SHA256";
pub const PROLOGUE: &[u8] = b"vessel/secure/1";
pub const PROOF_PROLOGUE: &[u8] = b"vessel/relay-proof/1";
pub const MAX_RECORD: usize = 65_535;
pub const DEFAULT_MAX_FRAME: u32 = 16 * 1024 * 1024;

pub struct NodeKeypair { /* secret: Zeroizing<[u8; 32]>, public: NodePublicKey */ }
impl NodeKeypair {
    pub fn from_secret(secret: [u8; 32]) -> Self;   // the 32 bytes of `node.key`
    pub fn generate() -> Result<Self, getrandom::Error>;
    pub fn public(&self) -> &NodePublicKey;
}
// NodePublicKey and RoutingId are vessel_proto's (see "Contract as built").
pub fn routing_id(key: &NodePublicKey) -> RoutingId;   // SHA-256(key)[..16]
pub fn fingerprint(key: &NodePublicKey) -> String;     // "300c 9c96 … bf92 4011"

/// Any carrier of whole binary messages, in order and reliably.
pub trait Carrier: Stream<Item = io::Result<Bytes>> + Sink<Bytes, Error = io::Error> + Unpin + Send + 'static {}
pub fn memory_pair(capacity: NonZeroUsize) -> (MemoryCarrier, MemoryCarrier);
pub struct LengthPrefixed<R, W>;                       // u16 big-endian length, then the message

pub async fn initiate<C: Carrier>(carrier: C, node: &NodePublicKey) -> Result<SecureChannel<C>, SecureError>;
pub async fn respond<C: Carrier>(carrier: C, keypair: &NodeKeypair) -> Result<SecureChannel<C>, SecureError>;
// initiate_with / respond_with take a Config { max_frame, handshake_timeout }.

/// Stream<Item = Result<String, SecureError>> + Sink<String, Error = SecureError>:
/// a FrameTransport.
pub struct SecureChannel<C>;

pub enum SecureError {
    Handshake(snow::Error), HandshakeMessage { len }, Timeout(Duration),
    Tampered(Tamper), TooLarge { len: u64, max: u32 }, NotUtf8, Exhausted,
    Closed, NotReady, Carrier(io::Error), Noise(snow::Error),
}
impl SecureError { pub fn is_transient(&self) -> bool; }  // Carrier, Timeout

// The relay's possession proof (S4).
pub fn proof_challenge(key: &NodePublicKey) -> Result<(ProofChallenge, HandshakeMessage), SecureError>;
impl ProofChallenge { pub fn verify(self, answer: &HandshakeMessage) -> bool; }
pub fn answer_proof(keypair: &NodeKeypair, challenge: &HandshakeMessage) -> Result<HandshakeMessage, SecureError>;
```

- Handshake: both messages exactly 48 bytes (an ephemeral key and an empty
  payload's tag); anything else is `HandshakeMessage`. One deadline, 10 s
  by default, covers the whole handshake.
  - A client with the wrong key gets no answer to read: NK lets only the
    node notice, and the node closes. The client sees the carrier end
    (`Carrier(UnexpectedEof)`, transient). An impostor's answer fails at
    the client with `Handshake`.
- Record and chunk format (ADR 0009):
  - one carrier message is one record of at most 65,535 bytes, the
    ChaCha20-Poly1305 ciphertext of one chunk;
  - a chunk's plaintext is `flags: u8` (`FIRST = 1`, `FINAL = 2`, other
    bits refused), then for `FIRST` the frame's length (`u32`,
    big-endian, at most 16 MiB), then data: at most 65,514 data bytes in a
    `FIRST` chunk, 65,518 in the others;
  - chunking is canonical: every chunk but a frame's last is full, so a
    frame has exactly one encoding and cannot be stretched into many tiny
    records;
  - frames are UTF-8 (`NotUtf8` otherwise).
- Every check happens before memory is allocated: record length, announced
  frame length, and each chunk against what is left. The buffer grows with
  the data, never past the announced length.
- A failed authentication tag (corrupted, replayed, reordered or dropped
  records), a reserved flag, a missing or misplaced `FIRST`, an overrun, an
  early or missing `FINAL`, or a short chunk closes the channel with
  `Tampered`, as do `TooLarge` from the peer, `NotUtf8` and `Exhausted`.
  Then the stream ends and sends fail with `Closed`. Refusing to send an
  oversized frame is the one error that leaves the channel open.
- A carrier that ends between frames ends the stream; one that ends inside
  a frame is `Carrier(UnexpectedEof)`.
- Fingerprint: the routing id's 16 bytes in eight groups of four hex
  digits (the sketch had 64 bits): one derived value, matching relay logs,
  and 128 bits leave no look-alike key to search for.
- Websocket carriers (axum's on the node, tungstenite's on the client)
  come with `vessel-relay` (S4, S5), which needs both anyway.

### `vessel-relay`

As built (Track S, 2026-11-06; `crates/vessel-relay`, the source of
truth). Relay endpoints (plain HTTP; `wss://` comes from a reverse proxy in
front):

```text
GET /v1/node                        websocket, the node's control connection
GET /v1/accept?conn=<id>            websocket, the node's data connection for one client
GET /v1/connect?node=<routing-id>   websocket, a client
GET /health                         "ok"
```

- **Control connection.** JSON text messages, defined in
  `vessel_proto::relay`:
  1. node: `Register { routing_id, key, token }`; the token is compared in
     constant time;
  2. relay: `Prove { message }`, the first message of a Noise NK handshake
     to `key` with prologue `vessel/relay-proof/1`; a routing id that is
     not the key's is refused with `Proof` without a challenge;
  3. node: `Proof { message }`, the response;
  4. relay: `Registered`, or `Rejected { reason }` and close;
  5. then, per waiting client, relay: `Incoming { conn }`, where `conn` is
     128 random bits, lowercase hex.
  - The node answers each `Incoming` by opening `/v1/accept?conn=…`.
  - A second registration for the same routing id replaces the first only
    after its own proof succeeds; the first connection is then closed.
  - Each registration step has 10 s.
- **Pairing.** The relay splices the client's and the node's data websocket
  and forwards binary messages unchanged, one at a time per direction.
  - It reads nothing from the client until the node's data connection is
    there.
  - A client waits at most 10 s for it (`4408`); at most 64 clients wait
    per node (`4429` beyond).
  - A close frame from either side is forwarded to the other unchanged
    (a node's `4401`, `vessel_proto::NodeClose`).
- **Close codes from the relay itself** (`vessel_proto::RelayClose`):
  `4404` no such node (transient), `4408` not accepted in time, `4413` a
  message too large, `4429` rate limited or too many waiting.
- **Limits** (`Limits`, defaults as the protocol says):
  - each message at most 65,535 bytes, checked from the frame header before
    the payload is read (control connections too);
  - every connection pinged every 10 s and dropped after 30 s of silence;
  - a token bucket per client address on `/v1/node` and `/v1/connect`
    (30 at once, then one per second; IPv6 counted by `/64`; at most 16,384
    addresses tracked, newcomers let in untracked beyond that). Behind a
    proxy the address comes from `RelayConfig::client_address_header`.

```rust
pub struct RelayConfig { pub token: RelayToken, pub limits: Limits, pub client_address_header: Option<HeaderName> }
pub async fn serve(listener: TcpListener, config: RelayConfig, shutdown: CancellationToken) -> io::Result<()>;

/// The node side: keep one relay registered (reconnecting with 0.5-30 s
/// jittered backoff, 30 s after a refusal), and hand every client's data
/// connection to `accept`.
pub struct NodeLink;
impl NodeLink {
    pub fn new(relay: RouteUrl, keypair: Arc<NodeKeypair>, token: RelayToken, tls: TlsConfig) -> Self;
    pub fn state(&self) -> watch::Receiver<LinkState>;   // Connecting, Registered, Refused(reason), Stopped
    pub async fn run(self, accept: impl Fn(WsCarrier) + Send + Sync + 'static, shutdown: CancellationToken);
}

/// The client side: a carrier to the node with `routing_id` through `relay`
/// (fails after 30 s of silence), or to a node's own `/secure` (no limit).
pub async fn dial(relay: &RouteUrl, node: &RoutingId, tls: &TlsConfig) -> Result<WsCarrier, DialError>;
pub async fn connect(url: &str, tls: &TlsConfig) -> Result<WsCarrier, DialError>;

/// Websocket carriers: opened (tungstenite) and accepted on `/secure` (axum).
pub struct WsCarrier;
pub struct AxumCarrier;   // AxumCarrier::new(socket, idle: Option<Duration>)
/// The node's handshake on a websocket: closes with 4401 when it fails.
pub async fn respond<C: NodeCarrier>(carrier: C, keypair: &NodeKeypair, config: &Config) -> Result<SecureChannel<C>, SecureError>;
/// The close code inside `SecureError::Carrier(io)`.
pub fn relay_close(error: &io::Error) -> Option<RelayClose>;
pub fn node_close(error: &io::Error) -> Option<NodeClose>;

/// rustls with `ring`, chosen explicitly; the OS roots, or given ones.
pub struct TlsConfig;     // native_roots(), with_roots(RootCertStore)
pub fn read_token_file(path: &Path) -> Result<RelayToken, TokenFileError>;
```

- `vesseld relay --bind ADDR --token-file PATH [--client-address-header
  NAME]` runs `serve` until a signal; it needs no data directory.
- New dependencies (AGENTS.md section 9): `rustls` (no default features;
  `ring`, `std`, `tls12`) and `rustls-native-certs`, for decision 1;
  `curve25519-dalek` (deriving a public key from `node.key` without a
  fallible path; already in the tree through `snow`) and `zeroize`
  (wiping the secret; already in the tree) in `vessel-secure`.

### `vessel-node`

- **`/secure`** on the existing listener:
  1. a websocket whose binary messages are the carrier, then `respond`;
  2. the preamble: `DeviceAuth` or `PairRequest`;
  3. `serve` as on every transport, with the device as the connection's
     principal.
  - Data connections from relays take the same path after the relay hands
    them over.
- **Relays and direct addresses**:
  - `vesseld --relay URL` (repeatable; also in node config) and
    `--relay-token-file PATH`.
  - `--public ADDR` (repeatable) lists the direct addresses a pairing link
    advertises. Nothing is guessed: a node behind NAT has none.
- **`device.*`, the scope check**, and closing a revoked device's open
  connections (each connection knows its principal).
- **`vesseld relay --bind ADDR --token-file PATH`**: the relay, with no data
  dir.

### `vessel-client` and the node book

```toml
[nodes.gpu-box]
id = "6f1c..."                         # pinned, as in M2
key = "x25519:<base64url>"             # pinned at pairing; the channel checks it
relay = ["wss://relay.example/vessel"] # tried in order
direct = ["ws://203.0.113.5:7077"]     # tried first, with a short timeout
credential = { file = "~/.config/vessel/credentials/6f1c....secret" }
```

- A node has `ssh`, `url`, or `key` with routes, exactly one of the three.
  `key` requires `credential` and at least one route.
- The project layer may still only set `default_node`.
- `SecureConnector`, a `Connector`, tries each direct address with a 3 s
  connect timeout, then each relay. Then it runs the handshake to `key` and
  `DeviceAuth`.
  - It is wrapped in `Pinned` like every connector, so the node id is checked
    too.
  - Errors are transient for a network failure, `4404`, `4408` or `4429`.
    They are final for a handshake failure (wrong key: never retried), and
    for `Refused` (`Revoked`, `UnknownDevice`).
- `pair(link, device_name) -> Result<PairedNode, PairError>`. The CLI writes
  the book entry and the credential file from the result.
- TLS for `wss://` relays (open decision 1). `ws://` relays and direct
  addresses need none, because the channel is end to end.

### `vessel-cli`

```text
vessel node pair [--label L] [--ttl 10m]         link (and QR) on stdout; needs `admin`
vessel nodes add NAME --pair LINK [--yes]        pair, write book and credential
vessel node devices                              list devices (id, label, last seen, revoked)
vessel node devices revoke ID                    revoke; its connections close
vessel node status                               shows the key fingerprint and routes
```

## Data flow

```text
laptop: vessel                     relay (vesseld relay)                  node: vesseld
  |                                    |  <== control ws: Register, Prove/Proof, Registered
  |-- GET /v1/connect?node=R --------->|
  |                                    |== Incoming{conn} ==============>|
  |                                    |<-- GET /v1/accept?conn=c -------|
  |<=========== spliced binary messages, unread by the relay ===========>|
  |-- Noise NK e, es ---------------------------------------------------->|  (key pinned at pairing)
  |<- Noise NK e, ee -----------------------------------------------------|
  |== [DeviceAuth] ====================================================> |  records, ChaCha20-Poly1305
  |<= [Authenticated] ====================================================|
  |== hello / calls / events, as on every transport =====================|
```

## Track S: secure channel and relay

Owns `vessel-secure`, `vessel-relay`, the `vesseld relay` subcommand, and the
testkit's misbehaving relay.

- **S1. `vessel-secure` handshake.** NK through `snow` (`default-features =
  false`, features `use-chacha20poly1305`, `use-sha2`, `use-curve25519`,
  `use-getrandom`; no `std`, so no `ring`). Tests:
  - the published NK vectors for our suite (`cacophony`, `noise-c`; Paseo
    keeps the same two in `packages/relay/src/noise/fixtures/`), checked
    with injected ephemerals;
  - a wrong key fails;
  - a wrong prologue fails;
  - the deadline fires.
- **S2. Records.** The chunk format, size checks before allocation,
  `FIRST`/`FINAL` rules, and teardown when the nonce counter reaches its
  maximum. Tests:
  - frames of 0 bytes, of exactly one record, and of 16 MiB round-trip;
  - 16 MiB + 1 is refused from the announced length alone;
  - every tampering closes the channel: corrupted, replayed, reordered,
    dropped or truncated records, reserved flags, an overrun;
  - a fuzz-style test feeds random records to the parser and checks it never
    panics and never allocates past the announced bound.
- **S3. Carriers.** Websocket binary messages, and a `u16`
  length-prefixed byte stream for later stdio use.
  - `SecureChannel` is a `FrameTransport`; `NodeClient::over` works on it
    unchanged.
- **S4. The relay server.** Endpoints, registration with the token and the
  possession proof, splicing without reading ahead, limits, close codes,
  `/health`. Tests:
  - an impostor registration with someone else's key fails the proof;
  - a wrong token is refused;
  - a client is not read before pairing;
  - the waiting cap, the wait timeout, and the rate limit;
  - a message over the limit closes with `4413`;
  - a bytes-captured test asserts the relay never sees a plaintext marker
    sent through the channel.
- **S5. `NodeLink` and `dial`.** Reconnect with backoff (0.5-30 s, as
  `follow_resuming`), pings, and replacing a half-open control connection.
  Tests restart the relay under a registered node and a waiting client.
- **S6. `vesseld relay`**, documented in `PROTOCOL.md` and the README
  (running a relay behind a reverse proxy for `wss://`).

## Track P: pairing, devices, node and client

Owns `vessel-store`, `vessel-node`, `vessel-client` and `vessel-cli` changes.

- **P1. Store.**
  - `node.key`, created on first start for new and existing data dirs.
  - The `devices` table (migration): the secret's hash, scopes, timestamps,
    `revoked_at` kept as a tombstone, so a revoked device hears `Revoked`
    and not `UnknownDevice`.
- **P2. Node.**
  - `/secure`, the preamble, and the principal per connection.
  - The scope check for every call; `device.*`; invitations in memory with
    TTL and single use.
  - Revocation closes the device's open connections.
  - One `NodeLink` per relay; `--public`; `NodeInfo.public_key`.
- **P3. Client.**
  - `SecureConnector` (route order, timeouts, transient vs final errors).
  - `pair()`; the book's new entry kind and its checks; credential files
    (0700 dir, 0600 files, named by node id).
  - The TLS client for `wss://` (open decision 1).
- **P4. CLI.**
  - `node pair` with link and QR; `nodes add --pair`, with fingerprint and
    confirmation.
  - `node devices` and `revoke`; status shows the key fingerprint and
    routes.
  - Attach over the relay: resume and messages as for SSH.
- **P5. Resume over the relay** (ADR 0007 rules, unchanged):
  - a relay drop fails calls in flight with `Disconnected` ("may or may not
    have been applied"); nothing is retried;
  - `follow_resuming` redials through `SecureConnector`, which takes the
    next route if the first is gone, and resumes from its cursor;
  - `4404` while the node re-registers is transient; `Revoked` ends the
    stream with its reason.

## Done criteria (milestone)

- Tests with no network beyond loopback, through an in-process relay and
  the testkit's misbehaving relay:
  - pairing:
    - over the relay with a link;
    - a second use of the same link, an expired link, and a link with the
      wrong key all fail;
  - `vessel --node gpu-box run` works end to end through the relay;
  - a relay drop during a terminal attach resumes with nothing repeated;
    `follow_resuming` moves to a second relay when the first is down;
  - a node with a direct address is used directly; with that address
    unreachable, through the relay;
  - a revoked device:
    - is refused with `Revoked`;
    - its open attach ends;
    - other devices are unaffected;
  - a relay that tampers with, replays or reorders records ends the
    connection, and no frame from it reaches the RPC layer;
  - an impostor registering another node's routing id fails the possession
    proof;
  - the relay's view of a session (captured bytes) holds no plaintext;
  - the published Noise NK vectors pass.
- The gate passes on Linux, macOS and Windows; relay and secure-channel
  tests run on all three.
- Manual smoke by the coordinator:
  - a relay on a public machine behind its HTTPS reverse proxy (`wss://`);
  - a node behind NAT; pair a laptop;
  - run, drop the network, resume;
  - revoke the laptop from the node.
- `PROTOCOL.md` has a version 4 entry. ADRs 0009 and 0010 are accepted.
  `ARCHITECTURE.md`, `README.md` and `docs/ROADMAP.md` describe relays and
  pairing.

## Contract as built (coordinator, 2026-11-06)

Protocol 4 is on `backend/m3` (see `docs/PROTOCOL.md`). Two deviations from
the seams below, decided while writing it:

- **Key types live in `vessel-proto`.** `NodePublicKey` (32 bytes, text
  `x25519:<base64url>`) and `RoutingId` (16 bytes) are wire data: they travel
  in `NodeInfo`, pairing links and relay registration, and wire types exist
  once (ADR 0001). `vessel-secure` therefore depends on `vessel-proto` for
  those types (a new edge, `vessel-secure -> proto`) and owns everything
  computed from them: deriving the routing id (truncated SHA-256), the
  fingerprint, the handshake. `NodeKeypair` stays in `vessel-secure`.
- **Routing ids and connection ids are lowercase hex**, not base32: URL-safe
  and no extra encoding dependency for 16 bytes.
- Preamble types are `vessel_proto::{Preamble, PreambleReply}`;
  `PreambleReply::Paired` boxes its `NodeInfo`.

## Decisions (coordinator, 2026-11-06)

All eight proposals below are accepted as written:

1. **TLS for `wss://` relays:** `rustls` with the `ring` provider, installed
   explicitly at startup (the M1 panic came from a missing provider), and the
   OS root certificates. This lets a relay sit behind an existing HTTPS
   proxy on 443, which is how TeamWicked would host it (Cloudflare and nginx
   in front of ingwprox1).
2. **QR codes:** `qrcode` 0.14 with default features off.
3. **Short typeable codes:** not in M3. A short code cannot carry a key, so it
   would need a PAKE.
4. **Relay registration:** one token per relay for nodes; clients need none
   and are rate limited.
5. **Default relay:** none built in; configured per node.
6. **Route order:** direct first with a 3 s timeout, then relays in order.
7. **Invitations:** in memory; a node restart voids pending ones.
8. **`snow` without a formal audit:** accepted. Its primitives are audited,
   and the published NK vectors plus our tamper and fuzz tests cover the
   composition.

ADRs 0009 and 0010 are accepted with these decisions. The protocol 4 contract
(below) is the coordinator's next change. Tracks start after it lands on
`backend/m3`.

## Open decisions (as drafted, kept for the record)

1. **TLS for `wss://` relays.**
   - Proposed: `rustls` with the `ring` crypto provider and the OS's root
     certificates (`rustls-native-certs`), chosen explicitly as ADR 0005's
     amendment requires.
   - Alternatives: `aws-lc-rs` (rustls's default; CMake and NASM on
     Windows), or the pure-Rust `rustls-rustcrypto` (younger).
   - Or no TLS client at all in M3, with relays reached as `ws://` on their
     own port. That rules out sitting behind an existing HTTPS reverse
     proxy on 443, the realistic deployment.
   - TLS adds no secrecy to the channel itself (ADR 0009).
2. **QR codes in the terminal.** Proposed: `qrcode` 0.14 with default
   features off (checked 2026-11-06: no dependencies, no `unsafe`), drawn
   with Unicode half blocks. Without it, the link only.
3. **Short manual codes.** Proposed: none in M3. A code short enough to
   type cannot carry a key, so it needs a PAKE (SPAKE2) and a rendezvous on
   the relay, a protocol of its own (ADR 0010).
4. **Who may register with a relay.**
   - Proposed: one registration token per relay, a secret shared with the
     nodes that use it. Clients need none, and are rate limited.
   - Alternatives: an open relay (anyone's free infrastructure), or one
     token per node (more to manage, for no gain while one owner runs it).
5. **Default relay.** Proposed: none. The owner configures their relay on
   each node (`--relay`) and pairing links carry it. Nothing in `vessel`
   points at a service we would have to keep running.
6. **Route order.** Proposed: direct addresses first (3 s connect timeout
   each), then relays in the order the link or book gives. Paseo probes
   every route and keeps the fastest; that is a later refinement, not
   needed for correctness.
7. **Invitations.** Proposed: in the node's memory, gone on restart. Ten
   minutes is too short to be worth a table.
8. **`snow` without a formal audit** (ADR 0009). Proposed: accept it,
   behind `vessel-secure`, with the published vectors and our own tamper and
   fuzz tests. The alternative, a hand-rolled NK on the same primitives, is
   also unaudited and is ours to get wrong.

## Grounding: what Paseo does (read 2026-11-06, ideas only)

Paths are in `getpaseo/paseo`.

- **Relay connections.** Both sides use
  `ws(s)://host/ws?serverId=…&role=server|client&v=2`
  (`packages/protocol/src/daemon-endpoints.ts`).
  - The daemon keeps a control socket and opens one data socket per client
    (`packages/server/src/server/relay-transport.ts`). On Cloudflare that is
    one Durable Object per daemon (`packages/relay/src/cloudflare-adapter.ts`).
  - The relay forwards frames unchanged, but replaces close codes.
  - It buffers up to 200 early client frames and drops the oldest.
  - It has no authentication and no rate limits. Anyone with a daemon's
    `serverId`, which is in its offer, can replace that daemon's control
    socket.
- **Crypto.** `Noise_NK_25519_ChaChaPoly_SHA256` with prologue
  `paseo-relay-e2ee/1` (`packages/relay/src/secure-channel.ts`, `noise/`).
  - The handshake payload carries a version byte and capability flags.
  - Nonces are an implicit counter; any failure closes the channel; there
    is no rekeying.
  - Records are chunked to fit Noise's 64 KiB limit, with
    `kind | flags | totalLength` in each chunk, and the claimed length is
    checked before allocating (`protected-records.ts`).
- **Two modes.** A legacy NaCl-box mode (random nonces, no replay
  protection) lives beside Noise. It needs two ephemerals per handshake,
  worst-case size budgets and downgrade guards. Removal is scheduled after
  2027-04 (`docs/relay-endpoint-security.md`). This is the
  `docs/LESSONS.md` lesson.
- **Pairing.**
  - The offer is `{v, serverId, daemonPublicKeyB64, relay}`, base64url JSON
    in `#offer=` (`packages/protocol/src/connection-offer.ts`,
    `packages/server/src/server/pairing-offer.ts`).
  - It is static and reusable. Every client is "owner"; devices get no
    credential and cannot be revoked individually, although
    `docs/permissions.md` describes invitations and principals.
  - The app confirms a new or changed host by a key fingerprint.
- **Permissions.** Nine semantic permissions, additive grants, and every
  operation mapped to its requirement in one exhaustive table
  (`packages/server/src/server/authorization/`). Daemon-to-daemon links
  intersect the caller's grants with the link's (`docs/connected-hosts.md`).
- **Route choice.** Every route is probed, and the client switches to one
  that is at least 40 ms faster three times in a row
  (`packages/app/src/runtime/host-runtime.ts`).
