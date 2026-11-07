# ADR 0010: Pairing links and device principals

Status: accepted, 2026-11-06, with the coordinator decisions in
`docs/plans/backend-m3.md`; built in milestone 3; amended 2026-11-06 after
Track R's review (a link only from stdin, a credential bound to its node's
id and key).

## Context

ADR 0009 gives every remote connection a Noise NK channel to the node's
static key. NK authenticates the node to the client, not the client to the
node: the client is anonymous until it says who it is inside the channel.
Two questions follow:

- How does a client learn the node's key, its id and where to reach it,
  without a channel that is already trusted?
- Who is the client, what may it do, and how is that taken back?

ADR 0005's node token answers neither for remote use. It is one secret per
node, every holder is indistinguishable, and revoking one holder means
rotating it for all. It stays for the machine's own clients (the `/rpc`
websocket on loopback and the local socket behind SSH).

What Paseo does (read 2026-11-06, ideas only; file references in the plan's
"Grounding"):

- A pairing offer `{v, serverId, daemonPublicKeyB64, relay}` is base64url
  JSON in a URL fragment (`https://app.paseo.sh/#offer=...`), also shown as
  a QR code. The fragment never reaches a web server.
- The offer is static and reusable ("treat it like a password"). Its docs
  describe expiring, single-use invitations and per-principal permissions,
  but the code admits every client as "owner" with every permission. A
  device gets no credential of its own, and the only revocation is rotating
  the daemon key or the password.
- Its permission model is nonetheless a good shape for later: a small set of
  semantic permissions, additive grants, and every operation classified by
  the permission it needs, so that a new operation cannot be added
  unclassified.
- The app confirms a new or changed host by showing a short key fingerprint.

## Options

### How the client learns the node

1. **A pairing link** carrying the node's id, public key and routes, made on
   the node and moved to the client by copy and paste or a QR code (Paseo).
2. **Trust on first use** through the relay: the client asks for the key
   and pins it. The first connection is then exactly as safe as the relay,
   which ADR 0009 keeps out of the trust base.
3. **A short code with a PAKE** (Magic Wormhole's SPAKE2): the user types a
   few words, and both sides derive the key exchange from them. Short and
   typeable, but a new protocol, a rendezvous on the relay, and a crate.

### How a device proves who it is

1. **The node token**, shared (ADR 0005). No per-device identity.
2. **A device credential**: a random secret per device, issued at pairing,
   sent inside the Noise channel, and stored hashed on the node.
3. **A static key per device**, authenticated by the handshake itself
   (Noise XK or IK instead of NK). Stronger in principle, but a second
   handshake pattern beside NK, against the "one mode" rule.

### What a paired device may do

1. **Everything** the node token allows.
2. **Semantic permissions** from the start (Paseo's nine).
3. **Everything for now, classified for later**: every protocol method
   declares the scope it needs; devices hold a set of scopes; M3 issues only
   the full set.

## Decision

### Pairing link (option 1)

- **Made on the node** by an authorized client: `vessel node pair` (local
  token, an SSH connection, or an already paired device with the `admin`
  scope) calls `device.invite { label, ttl }`. The node returns a link and
  keeps the invitation, by its hash, until it is used or expires.
- **Format.** `vessel://pair#<offer>`, where `<offer>` is base64url (no
  padding) of a versioned JSON object:

  ```json
  { "v": 1, "node": "<NodeId>", "key": "<base64url node public key>",
    "relay": ["wss://relay.example/vessel"], "direct": ["ws://203.0.113.5:7077"],
    "invite": "<base64url 16 random bytes>", "expires": "2026-11-06T12:30:00Z" }
  ```

  - As with Paseo, the payload sits in the fragment, so pasting the link into
    a browser sends nothing to a server.
  - `vessel` parses it; unknown fields are refused, and `v` is the only
    version check.
  - The CLI also prints it as a QR code in the terminal, so a phone (a later
    client) can scan it (the plan's open decision 2 settles the crate).
- **Expiring and single use.** Ten minutes by default, at most 24 hours. An
  invitation is consumed by the first successful pairing; a second use, an
  expired one, or an unknown one is refused with the same answer ("this
  invitation is not valid"), so an attacker learns nothing from the
  difference.
  - Invitations live in the node's memory. A restart voids those
    outstanding, which is acceptable for something that lives ten minutes.
- **Using it.** `vessel nodes add NAME --pair`, with the link pasted at
  its prompt or piped in. Never on the command line: other users of the
  machine can read a process's arguments, and shells keep them in their
  history, so someone else could redeem the link first (`--pair LINK` is
  refused, and the link called exposed; amended 2026-11-06 after Track R's
  review).
  1. The client parses the link, prints the node's key fingerprint, and asks
     to confirm when it is at a terminal (`--yes` otherwise).
  2. It connects (direct addresses first, then relays) and runs the Noise NK
     handshake to the key in the link: the key is pinned before anything is
     sent.
  3. It sends `PairRequest { invite, device_name }` as the first frame inside
     the channel.
  4. The node checks and consumes the invitation, creates the device, and
     answers `Paired { device_id, secret, node }`. The client accepts it
     only if `node` carries the link's id and key.
  5. The client writes the credential to a separate 0600 file, then the
     book entry (`key`, `id`, `relay`, `direct`). The book itself never
     holds a secret (ADR 0005's rule for tokens, and the book is meant to
     be hand-edited and shared between machines).
  - The credential file also names the node id and key from the link, and
    the client sends the secret to no other key or id (checked before it
    dials). Since the book is shared and hand-edited, an entry could name
    an attacker's key and relay while pointing at this node's credential
    file; the handshake to the attacker's key would succeed, so only the
    binding keeps the secret from them (amended 2026-11-06, Track R's
    review).
- **No short code in M3** (option 3 deferred). A PAKE is the only safe way to
  make a code short enough to type, since a short code cannot carry a
  32-byte key. It is a later addition that would sit beside the link, not
  replace it.

### Device credential (option 2)

- A device is `{ device_id, label, created_at, last_seen, scopes,
  secret_hash }`, stored in the node's database. `secret_hash` is
  SHA-256 of a 32-byte random secret: the secret itself is never stored, so
  a copied database authenticates nobody.
- Every connection's first frame inside the channel is either
  `DeviceAuth { device_id, secret }` or `PairRequest`. The node compares the
  hash in constant time, then answers `Authenticated` or
  `Refused { reason }` (an encrypted frame, so the reason survives relays,
  ADR 0009), and the protocol's `hello` follows.
- The secret travels only inside a channel already authenticated to the
  node's pinned key. That is why NK's anonymous client is enough, and why no
  second handshake pattern is needed.
- **Revocation.** `vessel node devices` lists devices; `vessel node devices
  revoke ID` (`device.revoke`) deletes one and closes its open connections at
  once. Its next connection is refused with a reason the client shows
  (`Refused { revoked }`), and a resuming stream stops instead of retrying,
  since this refusal is not transient.

### Authority (option 3)

- Scopes: `read` (node info, harnesses, sessions, logs, subscriptions),
  `operate` (create, send input, kill), `admin` (`node.stop`, `device.*`).
- Every method in the protocol's method table declares its scope. A new
  method that does not cannot be added: the table is exhaustive, like
  Paseo's operation map.
- M3 pairing grants all three, the same authority as the node token. A
  paired device is the owner's own machine. Narrower invitations (a
  `read`-only viewer, a device that may not manage devices) are an additive
  later change: `device.invite` gains a `scopes` field, and nothing already
  stored changes meaning.
- The node token and the local socket keep full authority.
- Agents calling back into the node get principals of their own in a later
  phase (ADR 0005's consequence). They will use the same scopes.

## Why

- **The link carries the key.** Pinning the key from the link removes the
  first-use window that trust on first use would leave open to the relay.
  The ten-minute single-use invitation bounds what a leaked link is worth.
  That is the gap between Paseo's documented design and its code.
- **Per-device secrets make revocation real.** One device can be cut off
  without touching the others or the node's key.
- **NK plus an in-channel credential** gives client authentication without a
  second Noise pattern, and keeps the "one mode" rule of ADR 0009.
- **Scopes from day one, granted fully.** The classification costs one
  column in a table we already generate. Without it, every later
  permission change would have to audit every method.
- **CLI-first.** The link is text, works over any chat or terminal, and the
  QR code is the same text for a camera.

## Consequences

- Protocol 4 adds `device.invite`, `device.list` and `device.revoke`, and
  the frames `DeviceAuth`, `PairRequest`, `Paired`, `Authenticated` and
  `Refused` before `hello` on secure channels. `NodeInfo` gains the node's
  public key, an additive field.
- The node's database gains a `devices` table (a migration).
- The client side keeps credentials in `<config_dir>/vessel/credentials/`
  (0700, files 0600), named by node id. Moving a device means pairing it
  again, not copying files, which is intended.
- A lost device is revoked from any other authorized client, or on the node
  itself with the token.
- A device's credential is bearer material on the device, as the node's
  private key is on the node. Hardware-backed storage (an OS keychain) is a
  later improvement.
