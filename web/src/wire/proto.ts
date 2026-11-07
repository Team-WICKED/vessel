// Generated from the Rust wire types (ADR 0017). Do not edit: running
// `VESSEL_WRITE_TS=1 cargo test --features ts typescript` in the crate that
// generates this file rewrites it, and CI fails while it differs.

/**
 * Empty success.
 */
export type Ack = null;

/**
 * One entry in a structured session's timeline.
 */
export type AgentEvent = { "event": "ready", 
/**
 * The harness's session id.
 */
harness_session: HarnessSessionId, } | { "event": "turn_started", 
/**
 * The prompt.
 */
content: Array<ContentBlock>, } | { "event": "user_message_chunk", 
/**
 * The chunk.
 */
content: ContentBlock, } | { "event": "message_chunk", 
/**
 * The chunk.
 */
content: ContentBlock, } | { "event": "thought_chunk", 
/**
 * The chunk.
 */
content: ContentBlock, } | { "event": "tool_call" } & ToolCall | { "event": "tool_call_update" } & ToolCallUpdate | { "event": "plan", 
/**
 * Plan entries in order.
 */
entries: Array<PlanEntry>, } | { "event": "permission_requested" } & PermissionRequest | { "event": "permission_resolved", 
/**
 * Which request.
 */
id: PermissionId, 
/**
 * How it was settled.
 */
outcome: PermissionOutcome, } | { "event": "turn_ended", 
/**
 * Why it stopped.
 */
reason: StopReason, } | { "event": "turn_failed", 
/**
 * Human-readable reason.
 */
message: string, } | { "event": "harness_log", 
/**
 * The line, without its newline.
 */
line: string, };

/**
 * Reply to `fleet.blob`.
 */
export type BlobChunk = { 
/**
 * Bytes from the requested offset: at most `MAX_LEN`, fewer at the end.
 */
data: string, 
/**
 * The blob's whole size, so the member knows when it is done.
 */
total: number, };

/**
 * The SHA-256 of a blob, lowercase hex on the wire. Blobs are fetched and
 * stored by it, so equal content is fetched once.
 */
export type BlobHash = string;

/**
 * A file in a bundle.
 */
export type BundleFile = { 
/**
 * Its content's hash.
 */
hash: BlobHash, 
/**
 * Its size in bytes.
 */
size: number, 
/**
 * Whether it is executable (scripts in skills). Ignored on Windows.
 * There is no other mode: members always write fleet files owner-only
 * (0600, 0700 if executable), so no manifest can widen access.
 */
executable?: boolean, };

/**
 * A file's place in a bundle: relative, `/`-separated, and valid as a file
 * name on every OS Vessel runs on, so joining it under a target directory
 * can never leave that directory.
 */
export type BundlePath = string;

/**
 * A request of any method, as sent on the wire.
 */
export type Call = { "method": "node.info", "params": GetNode } | { "method": "harness.list", "params": ListHarnesses } | { "method": "session.list", "params": ListSessions } | { "method": "session.get", "params": GetSession } | { "method": "session.create", "params": CreateSession } | { "method": "session.send", "params": SendInput } | { "method": "session.kill", "params": KillSession } | { "method": "session.read", "params": ReadEvents } | { "method": "session.subscribe", "params": Subscribe } | { "method": "session.unsubscribe", "params": Unsubscribe } | { "method": "node.stop", "params": StopNode } | { "method": "device.invite", "params": InviteDevice } | { "method": "device.list", "params": ListDevices } | { "method": "device.revoke", "params": RevokeDevice } | { "method": "profile.list", "params": ListProfiles } | { "method": "profile.reload", "params": ReloadProfiles } | { "method": "session.move_profile", "params": MoveProfile } | { "method": "fleet.manifest", "params": GetManifest } | { "method": "fleet.blob", "params": ReadBlob } | { "method": "fleet.watch", "params": WatchFleet } | { "method": "fleet.report", "params": ReportApplied } | { "method": "fleet.members", "params": ListMembers } | { "method": "fleet.forward", "params": Forward } | { "method": "fleet.relay", "params": RelayCall } | { "method": "fleet.deliver", "params": Deliver } | { "method": "fleet.grant", "params": GrantHome } | { "method": "fleet.callback", "params": GetCallback } | { "method": "fleet.peers", "params": ListPeers } | { "method": "fleet.nodes", "params": ListNodes } | { "method": "fleet.forget", "params": ForgetNode } | { "method": "session.export", "params": ExportSession } | { "method": "session.import", "params": ImportSession } | { "method": "memory.export", "params": ExportMemory } | { "method": "memory.import", "params": ImportMemory } | { "method": "transfer.read", "params": ReadTransfer } | { "method": "transfer.close", "params": CloseTransfer } | { "method": "secret.set", "params": SetSecret } | { "method": "secret.list", "params": ListSecrets } | { "method": "secret.delete", "params": DeleteSecret } | { "method": "secret.copy", "params": CopySecret } | { "method": "model.list", "params": ListModels };

/**
 * What `fleet.callback` answers.
 */
export type Callback = { 
/**
 * The relays the home registers with, in its preference order; none
 * when the home has no relay (it is reached directly), and then a
 * member needs a route of its own to be called back.
 */
relays: Array<RouteUrl>, 
/**
 * A ticket for the member's key, with `relays`.
 */
ticket?: RelayTicket | null, };

/**
 * `transfer.close` (protocol 6): drop the snapshot. Only the caller that
 * opened it may close it. Needs `read`.
 */
export type CloseTransfer = { 
/**
 * Which transfer.
 */
transfer: TransferId, };

/**
 * A value in an MCP server definition: literal text, or the name of a
 * secret each node resolves from its own store. On the wire, a string or
 * `{"secret": "NAME"}`.
 */
export type ConfigValue = string | { secret: SecretName };

/**
 * A piece of message content.
 */
export type ContentBlock = { "type": "text", 
/**
 * The text.
 */
text: string, } | { "type": "resource_link", 
/**
 * URI of the resource.
 */
uri: string, 
/**
 * Display name.
 */
name: string, } | { "type": "opaque", 
/**
 * The block as the transport sent it.
 */
raw: RawJson, };

/**
 * `secret.copy` (protocol 6): send this node's value for `name` to fleet
 * node `to`, which stores it under the same name. Needs `admin` here, and
 * the delivering grant on `to` must hold `admin` too.
 */
export type CopySecret = { 
/**
 * Which secret.
 */
name: SecretName, 
/**
 * The node to copy it to.
 */
to: NodeId, };

/**
 * `session.create`: start a harness.
 */
export type CreateSession = { 
/**
 * What to run.
 */
spec: SessionSpec, };

/**
 * Who created a session (ADR 0013): the owner, a paired device, or an agent
 * running in another session.
 */
export type Creator = { "kind": "owner" } | { "kind": "device", 
/**
 * The device.
 */
id: DeviceId, } | { "kind": "agent", 
/**
 * The node the agent's session runs on.
 */
node: NodeId, 
/**
 * The agent's session.
 */
session: SessionId, };

/**
 * `secret.delete` (protocol 6): remove a secret. Needs `admin`.
 */
export type DeleteSecret = { 
/**
 * Which secret.
 */
name: SecretName, };

/**
 * `fleet.deliver` (protocol 6): the home asks a member to run a call for
 * `origin`. The last hop: a member never passes a delivered call on, and
 * accepts one only from its home's device (the one `fleet.grant` made).
 * Needs `read`; the inner call runs with the delivering device's grant
 * intersected with `origin.scopes`.
 */
export type Deliver = { 
/**
 * The call.
 */
call: Call, 
/**
 * Who it speaks for.
 */
origin: Origin, };

/**
 * A paired device: one client machine holding its own credential for a
 * node (ADR 0010). Minted by the node at pairing.
 */
export type DeviceId = string;

/**
 * A paired device as the node reports it.
 */
export type DeviceInfo = { 
/**
 * Its id.
 */
id: DeviceId, 
/**
 * The name given at pairing (the client machine's name by default).
 */
label: string, 
/**
 * When it was paired.
 */
created_at: string, 
/**
 * When it last authenticated.
 */
last_seen?: string | null, 
/**
 * What it may do.
 */
scopes: Array<Scope>, 
/**
 * When it was revoked, if it was.
 */
revoked_at?: string | null, };

/**
 * A duration in whole milliseconds.
 */
export type DurationMs = number;

/**
 * An environment variable's name.
 */
export type EnvName = string;

/**
 * What kind of failure occurred. Clients branch on this.
 */
export type ErrorCode = "invalid_request" | "unauthorized" | "forbidden" | "protocol_mismatch" | "not_found" | "conflict" | "unsupported" | "harness_not_installed" | "spawn_failed" | "unavailable" | "internal";

/**
 * Reply to `session.read`.
 */
export type EventPage = { 
/**
 * Records in order, starting at the requested `from` (or the oldest
 * retained record if older ones were pruned).
 */
records: Array<EventRecord>, 
/**
 * Sequence number to ask for next.
 */
next: EventSeq, 
/**
 * Whether `next` is the end of the log right now.
 */
at_end: boolean, };

/**
 * An event with its position and time; the unit of the session log.
 */
export type EventRecord = { 
/**
 * Position in the session log.
 */
seq: EventSeq, 
/**
 * When the node recorded it.
 */
at: string, 
/**
 * What happened.
 */
event: SessionEvent, };

/**
 * Position of an event in one session's log. Starts at zero and has no
 * gaps, so a client resumes with "everything from seq N".
 */
export type EventSeq = number;

/**
 * `memory.export` (protocol 6): snapshot a profile's memory, one
 * project's or all of it. Needs `admin`: memory is kept per account on
 * purpose (plan decision 4).
 */
export type ExportMemory = { 
/**
 * Whose memory; absent is the harness's default profile.
 */
profile?: ProfileName | null, 
/**
 * One project's, by its working directory on this node; absent is
 * every project.
 */
project?: string | null, };

/**
 * `session.export` (protocol 6): snapshot a structured session's harness
 * files (never its profile's memory) into a transfer. A session mid-turn is
 * refused unless `force`, as for `session.move_profile`. An `operate` call
 * on that session, so an agent exports only what it started. Needs
 * `operate`.
 */
export type ExportSession = { 
/**
 * Which session.
 */
session: SessionId, 
/**
 * Snapshot even while a turn runs.
 */
force?: boolean, };

/**
 * A member as its home reports it.
 */
export type FleetMember = { 
/**
 * The member's device on the home.
 */
device: DeviceId, 
/**
 * Its label.
 */
label: string, 
/**
 * The generation it last applied, if it has reported.
 */
applied?: Generation | null, 
/**
 * Files it kept as edited.
 */
conflicts?: Array<BundlePath>, 
/**
 * When it last reported.
 */
reported_at?: string | null, };

/**
 * A node of a fleet, the home included.
 */
export type FleetNode = { 
/**
 * Its id.
 */
node: NodeId, 
/**
 * Its name.
 */
name: string, 
/**
 * The protocol its home speaks with it (a protocol 5 member gets the
 * bundle but no delivered calls).
 */
protocol: ProtocolVersion, 
/**
 * Whether the home can reach it now.
 */
reachable: boolean, };

/**
 * `fleet.forget` (protocol 6): on the home, forget member node `node`'s
 * binding (its key and name, kept for good otherwise), for a node
 * reinstalled with a new key or to be renamed. Its grants are deleted;
 * the invitations outstanding on the home are withdrawn, and the node's
 * id and name go next only to a member device paired after this, so the
 * node is invited again and joins from there. Needs `admin`; never
 * forwarded.
 */
export type ForgetNode = { 
/**
 * The member node.
 */
node: NodeId, };

/**
 * `fleet.forward` (protocol 6): run `call` on fleet node `node`. Sent by a
 * client to its own node, which checks `call` against the caller as if it
 * were local, then passes it on with the caller's [`Origin`]. Needs `read`
 * here; the inner call's own scope is checked on every hop.
 */
export type Forward = { 
/**
 * The node that runs the call; this node itself runs it locally.
 */
node: NodeId, 
/**
 * The call, which must be [forwardable](Method::forwardable).
 */
call: Call, };

/**
 * Reply to `fleet.forward`, `fleet.relay` and `fleet.deliver`: the inner
 * call's reply. An inner call that fails fails the envelope with its error.
 */
export type Forwarded = { 
/**
 * What the target answered.
 */
reply: Reply, };

/**
 * A bundle generation. The home increases it by one each time it
 * publishes; members compare it to what they last applied.
 */
export type Generation = number;

/**
 * `fleet.callback` (protocol 6): what a member with no route of its own
 * needs for its home to call it back: the relays the home registers with,
 * and a ticket that registers this member's own node key there for a while
 * (never the relays' token). The home answers only a member device, for
 * the node and key its record binds that device to, or, before its first
 * grant, for a claim its record would take. Asked again before the ticket
 * expires. Needs `sync`; never forwarded.
 */
export type GetCallback = { 
/**
 * The member's node.
 */
node: NodeId, 
/**
 * Its key, which the ticket names.
 */
key: NodePublicKey, };

/**
 * `fleet.manifest` (protocol 5): the home's current bundle. Needs `Sync`.
 */
export type GetManifest = null;

/**
 * `node.info`: describe the node.
 */
export type GetNode = null;

/**
 * `session.get`: one session.
 */
export type GetSession = { 
/**
 * Which session.
 */
session: SessionId, };

/**
 * `fleet.grant` (protocol 6): a member gives its home a credential on
 * itself, as a single-use pairing link made with its own `device.invite`
 * and the scopes its owner chose. The home redeems it like any device and
 * keeps the credential; revoking it on the member cuts the home off.
 * Needs `sync`.
 */
export type GrantHome = { 
/**
 * The link, made on the member.
 */
link: PairingLink, };

/**
 * A harness found on a node.
 */
export type Harness = { 
/**
 * Which harness.
 */
kind: HarnessKind, 
/**
 * Resolved executable path.
 */
path: string, 
/**
 * First line of `<binary> --version`, if it answered in time.
 */
version: string | null, 
/**
 * Structured-mode availability. Terminal mode is always available for an
 * installed harness.
 */
structured: StructuredSupport, };

/**
 * An agent harness Vessel knows how to drive.
 *
 * Closed on purpose: everything harness-specific (argv, environment,
 * structured-mode launch) lives in one module per variant in
 * `vessel-harness`, and adding a variant must fail to compile until that
 * module exists.
 */
export type HarnessKind = "claude-code" | "codex" | "gemini" | "open-code";

/**
 * The harness's own session id (an ACP `sessionId`). Kept so a
 * structured session can be resumed by the harness after a node restart.
 */
export type HarnessSessionId = string;

/**
 * An HTTP header field name.
 */
export type HeaderName = string;

/**
 * An `http://` or `https://` URL with a host and no credentials in it.
 *
 * A URL is public: it travels in manifests and, for some harnesses, on a
 * command line that every local user can read (`ps`). So `user:pass@` is
 * refused; a credential goes in a header, as a secret reference
 * (`ConfigValue::Secret`). A token in the query string cannot be told from
 * an ordinary parameter, so the same rule is stated here rather than
 * enforced: keep secrets out of URLs.
 */
export type HttpUrl = string;

/**
 * `memory.import` (protocol 6): the same pull for memory. A file that
 * differs from one already there is written beside it
 * (`.vessel-incoming`), as the fleet's materialiser does; nothing is
 * overwritten. Needs `admin`.
 */
export type ImportMemory = { 
/**
 * The memory to copy.
 */
from: MemoryRef, 
/**
 * The profile here; absent is the default.
 */
profile?: ProfileName | null, 
/**
 * The project here, when copying one; absent is the source's.
 */
project?: string | null, };

/**
 * `session.import` (protocol 6): the target pulls a session from `from`
 * (exporting it there, through `fleet.forward` when it is another node),
 * places its files in the chosen profile's home, renamed for the working
 * directory here, and with `start` resumes it as a new session. A file
 * already there with other content refuses the import before anything is
 * written. Moving is importing, then killing the source. Needs `operate`.
 */
export type ImportSession = { 
/**
 * The session to copy.
 */
from: SessionRef, 
/**
 * The profile here; absent is the harness's default.
 */
profile?: ProfileName | null, 
/**
 * The working directory here; absent is the source's.
 */
cwd?: string | null, 
/**
 * Start a session resuming the conversation.
 */
start?: boolean, };

/**
 * Reply to `device.invite`.
 */
export type Invitation = { 
/**
 * The link to hand to the new device.
 */
link: PairingLink, 
/**
 * When it expires.
 */
expires: string, };

/**
 * The single-use secret of a pairing invitation: 128 random bits.
 */
export type InvitationSecret = string;

/**
 * `device.invite` (protocol 4): make a pairing link. Needs `Admin`.
 */
export type InviteDevice = { 
/**
 * Label for the device that redeems it (it may name itself instead).
 */
label?: string | null, 
/**
 * How long the invitation is valid, in seconds (the node caps it).
 */
ttl_secs: number, 
/**
 * What the device may do (protocol 5); `None` is
 * [`Scope::DEVICE_DEFAULT`], what every protocol 4 device got. A fleet
 * member is invited with `[sync]`.
 */
scopes?: Array<Scope> | null, };

/**
 * `session.kill`: stop a session's process (and its process group).
 */
export type KillSession = { 
/**
 * Which session.
 */
session: SessionId, };

/**
 * `device.list` (protocol 4): every device, revoked ones included.
 */
export type ListDevices = null;

/**
 * `harness.list`: discover installed harnesses.
 */
export type ListHarnesses = null;

/**
 * `fleet.members` (protocol 5): the home's members and what each last
 * reported. Needs `Admin`.
 */
export type ListMembers = null;

/**
 * `model.list` (protocol 7): the routes this node offers, for pickers and
 * `vessel models`. Needs `read`.
 */
export type ListModels = null;

/**
 * `fleet.nodes` (protocol 6): what this node knows of its fleet (a member
 * asks its home with `fleet.peers`). Needs `read`.
 */
export type ListNodes = null;

/**
 * `fleet.peers` (protocol 6): the home's view of its fleet, for its
 * members. Needs `sync`.
 */
export type ListPeers = null;

/**
 * `profile.list` (protocol 5): the node's profiles, the implicit default
 * of each harness included. Needs `Read`.
 */
export type ListProfiles = { 
/**
 * Only this harness's profiles.
 */
harness?: HarnessKind | null, };

/**
 * `secret.list` (protocol 6): the names of this node's secrets, never
 * their values. Needs `admin`.
 */
export type ListSecrets = null;

/**
 * `session.list`: every session the node knows, newest first, including
 * ended sessions restored from disk.
 */
export type ListSessions = null;

/**
 * What a home publishes: every file by its path, and the MCP servers with
 * secret references unresolved.
 */
export type Manifest = { 
/**
 * Which publication this is.
 */
generation: Generation, 
/**
 * The files.
 */
files: { [key in BundlePath]: BundleFile }, 
/**
 * The MCP servers.
 */
mcp: { [key in McpServerName]: McpTransport<ConfigValue> }, };

/**
 * An MCP server's name, unique within a fleet's definitions.
 */
export type McpServerName = string;

/**
 * How a harness reaches an MCP server. `V` is `ConfigValue` in definitions
 * and `ResolvedValue` at launch; only environment variables and headers
 * hold one, so a secret can never become an argument.
 */
export type McpTransport<V> = { "type": "stdio", 
/**
 * The program, found on the launching node's `PATH` if relative.
 */
command: string, 
/**
 * Its arguments, literal only.
 */
args?: Array<string>, 
/**
 * Its environment.
 */
env?: { [key in EnvName]: V }, } | { "type": "http", 
/**
 * Its endpoint.
 */
url: HttpUrl, 
/**
 * Request headers (credentials usually go here).
 */
headers?: { [key in HeaderName]: V }, };

/**
 * Reply to `memory.import`.
 */
export type MemoryImported = { 
/**
 * Files written.
 */
written: number, 
/**
 * Files already there with the same content.
 */
same: number, 
/**
 * Files that differed, written beside the ones there.
 */
beside?: Array<BundlePath>, };

/**
 * A profile's memory on a node of the fleet.
 */
export type MemoryRef = { 
/**
 * The node.
 */
node: NodeId, 
/**
 * Whose memory there; absent is the default profile.
 */
profile?: ProfileName | null, 
/**
 * One project's, by its working directory there; absent is all.
 */
project?: string | null, };

/**
 * Every RPC method.
 */
export type Method = "node.info" | "harness.list" | "session.list" | "session.get" | "session.create" | "session.send" | "session.kill" | "session.read" | "session.subscribe" | "session.unsubscribe" | "node.stop" | "device.invite" | "device.list" | "device.revoke" | "profile.list" | "profile.reload" | "session.move_profile" | "fleet.manifest" | "fleet.blob" | "fleet.watch" | "fleet.report" | "fleet.members" | "fleet.forward" | "fleet.relay" | "fleet.deliver" | "fleet.grant" | "fleet.callback" | "fleet.peers" | "fleet.nodes" | "fleet.forget" | "session.export" | "session.import" | "memory.export" | "memory.import" | "transfer.read" | "transfer.close" | "secret.set" | "secret.list" | "secret.delete" | "secret.copy" | "model.list";

/**
 * An API a provider or a harness speaks.
 */
export type ModelApi = "anthropic_messages" | "openai_responses" | "openai_chat_completions";

/**
 * One call a session made to a model through the node's proxy: metadata
 * only, never prompt or completion content, tool arguments or keys.
 */
export type ModelCall = { 
/**
 * The route the session runs on.
 */
route: ModelRouteName, 
/**
 * The provider's model that was called.
 */
model: ModelId, 
/**
 * The API the provider speaks.
 */
api: ModelApi, 
/**
 * The tokens it used (zero where the provider said nothing).
 */
usage: TokenUsage, 
/**
 * How it ended.
 */
stop: ModelStop, 
/**
 * Why it failed, when it did.
 */
failure?: ModelFailure | null, 
/**
 * From the proxy's first byte out to the answer's end.
 */
duration_ms: DurationMs, };

/**
 * Why a model call failed: its class, never the provider's message.
 */
export type ModelFailure = "rate_limited" | "overloaded" | "upstream_error" | "refused_request" | "context_too_long" | "timeout" | "truncated" | "unauthorized";

/**
 * A model name as the harness understands it (`opus`, `gpt-5`, ...).
 * The set is open and harness-specific, so it is not an enum.
 */
export type ModelId = string;

/**
 * A route a node offers (`model.list`, protocol 7).
 */
export type ModelRoute = { 
/**
 * Its name, which profiles use.
 */
name: ModelRouteName, 
/**
 * The provider it calls.
 */
provider: ProviderName, 
/**
 * The API that provider speaks.
 */
api: ModelApi, 
/**
 * The provider's model.
 */
model: ModelId, 
/**
 * The provider's model for a harness's small, fast calls.
 */
small_model?: ModelId | null, };

/**
 * A model route's name in a node's `models.toml`, which profiles name
 * with `model = "..."` (protocol 7).
 */
export type ModelRouteName = string;

/**
 * How a model call ended.
 */
export type ModelStop = "end_turn" | "tool_use" | "max_tokens" | "stop_sequence" | "refusal" | "pause" | "error";

/**
 * `session.move_profile` (protocol 5, ADR 0011): restart a structured
 * session's harness under another profile, resuming the same conversation.
 * Refused mid-turn unless `force`, and refused when the target profile is
 * not logged in. Answers with the session as it is now. Needs `Operate`.
 */
export type MoveProfile = { 
/**
 * Which session.
 */
session: SessionId, 
/**
 * The profile to move to; `None` is the harness's default.
 */
profile?: ProfileName | null, 
/**
 * Move even in the middle of a turn, cancelling it.
 */
force?: boolean, };

/**
 * What `fleet.forget` did.
 */
export type NodeForgotten = { 
/**
 * The key the node was bound to.
 */
key: NodePublicKey, 
/**
 * The name it was known by, now released.
 */
name?: string | null, 
/**
 * The member devices that granted for it; their grants are deleted.
 * One that should not sync any more is revoked separately.
 */
devices: Array<DeviceId>, 
/**
 * Invitations that were outstanding on the home and are withdrawn.
 */
withdrawn: number, };

/**
 * Identity of a Vessel node; one per machine, stable across restarts.
 */
export type NodeId = string;

/**
 * Facts about a node.
 */
export type NodeInfo = { 
/**
 * Stable id for this node.
 */
id: NodeId, 
/**
 * Human-readable name (defaults to the hostname).
 */
name: string, 
/**
 * OS hostname.
 */
hostname: string, 
/**
 * `std::env::consts::OS`.
 */
os: string, 
/**
 * `std::env::consts::ARCH`.
 */
arch: string, 
/**
 * Version of the node binary.
 */
version: string, 
/**
 * Protocols the node speaks.
 */
protocol: ProtocolRange, 
/**
 * When the node process started.
 */
started_at: string, 
/**
 * Whether it takes new sessions (protocol 3; absent means serving).
 */
state: NodeState, 
/**
 * The node's static key for secure channels (protocol 4; absent from
 * older nodes and from nodes without a key yet).
 */
public_key?: NodePublicKey | null, };

/**
 * A node's static X25519 public key: the identity a client pins at pairing
 * and checks on every secure handshake.
 */
export type NodePublicKey = string;

/**
 * Whether a node is taking new work.
 */
export type NodeState = "serving" | "draining";

/**
 * The pairing-link format version. Only version 1 exists; a link from a
 * newer format is refused with a clear error rather than half-understood.
 */
export type OfferVersion = 1;

/**
 * Who a forwarded call speaks for, as the node it started on vouches.
 */
export type Origin = { 
/**
 * The node the call started on.
 */
node: NodeId, 
/**
 * The principal there.
 */
by: OriginBy, 
/**
 * What that principal holds on its own node. The target never grants
 * more than these, whatever its own grant to the delivering device.
 */
scopes: Array<Scope>, };

/**
 * The principal a call started with, on its own node.
 */
export type OriginBy = { "kind": "owner" } | { "kind": "device", 
/**
 * Its id on that node.
 */
id: DeviceId, } | { "kind": "agent", 
/**
 * The agent's session.
 */
session: SessionId, 
/**
 * How many agents deep that session is (the owner's own sessions
 * are 0), so the target can refuse a session past the depth limit.
 */
depth: number, };

/**
 * A pairing link: `vessel://pair#` followed by the base64url JSON of a
 * [`PairingOffer`]. The offer sits after `#` so that, if a link is ever
 * opened through a web page, the fragment never reaches a server.
 */
export type PairingLink = string;

/**
 * Everything a client needs to pair with a node, carried by a pairing link.
 *
 * Unknown fields are refused (ADR 0010): an offer is security-relevant, so a
 * field this version does not understand must not be silently ignored;
 * format changes bump `v` instead.
 */
export type PairingOffer = { 
/**
 * Format version (always 1).
 */
v: OfferVersion, 
/**
 * The node's id (pinned like an SSH node's).
 */
node: NodeId, 
/**
 * The node's static key (pinned; the handshake checks it).
 */
key: NodePublicKey, 
/**
 * Relays the node is registered with, in preference order.
 */
relays: Array<RouteUrl>, 
/**
 * Direct addresses the node was told to advertise (`--public`).
 */
direct: Array<RouteUrl>, 
/**
 * The single-use invitation.
 */
invite: InvitationSecret, 
/**
 * When the invitation expires.
 */
expires: string, };

/**
 * A pending permission request inside a structured session. Minted by the
 * node (not the harness) so many clients can answer the same request and
 * the first answer wins.
 */
export type PermissionId = string;

/**
 * One choice offered by a permission request.
 */
export type PermissionOption = { 
/**
 * Harness-assigned option id, echoed back when chosen.
 */
id: PermissionOptionId, 
/**
 * Label to show.
 */
name: string, 
/**
 * Meaning.
 */
kind: PermissionOptionKind, };

/**
 * A permission option id offered by the harness (for example
 * `allow-once`). Echoed back verbatim when a client picks it.
 */
export type PermissionOptionId = string;

/**
 * What choosing an option means.
 */
export type PermissionOptionKind = "allow_once" | "allow_always" | "reject_once" | "reject_always";

/**
 * How a permission request was settled.
 */
export type PermissionOutcome = { "outcome": "selected", 
/**
 * The chosen option.
 */
option: PermissionOptionId, } | { "outcome": "cancelled" };

/**
 * A request for human approval, pending until a client answers.
 */
export type PermissionRequest = { 
/**
 * Node-assigned id clients answer with.
 */
id: PermissionId, 
/**
 * The tool call needing approval.
 */
tool_call: ToolCallId, 
/**
 * Choices, in the harness's order.
 */
options: Array<PermissionOption>, };

/**
 * One step of the agent's plan.
 */
export type PlanEntry = { 
/**
 * What the step is.
 */
content: string, 
/**
 * Importance.
 */
priority: PlanPriority, 
/**
 * Progress.
 */
status: PlanStatus, };

/**
 * Priority of a plan entry.
 */
export type PlanPriority = "high" | "medium" | "low";

/**
 * Progress of a plan entry.
 */
export type PlanStatus = "pending" | "in_progress" | "completed";

/**
 * A profile as a node reports it. Names of the variables it sets are
 * listed; their values never leave the node (they may be secrets or paths
 * to credentials).
 */
export type ProfileInfo = { 
/**
 * The profile's name.
 */
name: ProfileName, 
/**
 * A label for people (for example the account it logs in as).
 */
label?: string | null, 
/**
 * The harness it applies to.
 */
harness: HarnessKind, 
/**
 * Names of the environment variables it sets.
 */
sets: Array<EnvName>, 
/**
 * The model route it names (`model = "..."` in `profiles.toml`,
 * protocol 7): its sessions' harness reaches that route's model
 * through the node's proxy. Absent for a profile on the harness's own
 * provider, and from older nodes.
 */
model?: ModelRouteName | null, };

/**
 * A profile's name: what `--profile` and `profiles.toml` use.
 */
export type ProfileName = string;

/**
 * An inclusive range of protocol versions a peer can speak.
 *
 * [Decision Log]
 * - Purpose: fleet nodes, the CLI and future editor plugins will run
 *   different versions at the same time.
 * - Alternatives: exact-match versions (Zed's collab server answers 426 on
 *   any mismatch, workable only because Zed ships client and server
 *   together); no envelope version at all (Paseo, which now carries hundreds
 *   of per-field compatibility shims).
 * - Choice: both sides state a range in the handshake and use the highest
 *   common version, or refuse with `ErrorCode::ProtocolMismatch`.
 */
export type ProtocolRange = { 
/**
 * Oldest supported version.
 */
min: ProtocolVersion, 
/**
 * Newest supported version.
 */
max: ProtocolVersion, };

/**
 * A wire protocol version.
 */
export type ProtocolVersion = number;

/**
 * A model provider's name in a node's `models.toml` (protocol 7).
 */
export type ProviderName = string;

/**
 * A JSON value carried as text and never inspected.
 *
 * In TypeScript it is `unknown` (ADR 0017): the one type of the wire with
 * no shape to declare. A page narrows it before use, as Vessel would have
 * to define a typed struct.
 */
export type RawJson = unknown;

/**
 * `fleet.blob` (protocol 5): part of a blob, from `offset`. A member asks
 * only for blobs it lacks, chunk by chunk. Needs `Sync`.
 */
export type ReadBlob = { 
/**
 * Which blob.
 */
hash: BlobHash, 
/**
 * Where to start, in bytes.
 */
offset: number, };

/**
 * `session.read`: a page of a session's log. The authoritative way to fill
 * gaps; live subscriptions are an optimisation on top of it.
 */
export type ReadEvents = { 
/**
 * Which session.
 */
session: SessionId, 
/**
 * First sequence number wanted.
 */
from: EventSeq, 
/**
 * Maximum number of records; the node may return fewer and caps it.
 */
limit: number, 
/**
 * With nothing at `from` yet, wait this many seconds for something to
 * arrive before answering (an empty page when nothing did), at most
 * [`ReadEvents::MAX_WAIT`] (protocol 6). A long poll, so waiting on
 * another node's session needs no subscription across hops.
 */
wait?: number | null, };

/**
 * `transfer.read` (protocol 6): part of one file of an open transfer, from
 * `offset`, as `fleet.blob` reads the bundle. Only the caller that opened
 * it may read it. Needs `read`. Answers a `BlobChunk`.
 */
export type ReadTransfer = { 
/**
 * Which transfer.
 */
transfer: TransferId, 
/**
 * Which file, by content.
 */
hash: BlobHash, 
/**
 * Where to start, in bytes.
 */
offset: number, };

/**
 * `fleet.relay` (protocol 6): a member asks its home to pass a call on.
 * The home refuses an `origin.node` other than the member's own (a member
 * speaks only for its own principals). Needs `relay`, which a member gets
 * only when the home's owner asks for it.
 */
export type RelayCall = { 
/**
 * The node that runs the call; the home runs it itself when it is
 * the target.
 */
node: NodeId, 
/**
 * The call.
 */
call: Call, 
/**
 * Who it speaks for.
 */
origin: Origin, };

/**
 * A registration ticket (protocol 6, `fleet.callback`): lets the one node
 * whose key it names register with a relay until it expires, without the
 * relay's token. A fleet's home mints one for a member from the token it
 * registers with, and the relay checks it with the same token, so the
 * relay keeps no state for it and rotating the token voids every ticket.
 */
export type RelayTicket = { 
/**
 * The node it registers: the relay takes it only with this key, and
 * only once the node proved it holds the key.
 */
key: NodePublicKey, 
/**
 * When it stops registering the node, in seconds since the Unix epoch;
 * a relay also ends a registration made with it then.
 */
expires_at: number, 
/**
 * The MAC over the key and `expires_at`.
 */
mac: TicketMac, };

/**
 * `profile.reload` (protocol 5): read `profiles.toml` again. If it does
 * not parse, the node keeps the profiles it had and the error says where.
 * Answers with the profiles now in effect. Needs `Admin`.
 */
export type ReloadProfiles = null;

/**
 * The owner or a device of another node, for [`SessionOrigin::Remote`].
 */
export type RemoteCreator = { "kind": "owner" } | { "kind": "device", 
/**
 * Its id on that node.
 */
id: DeviceId, };

/**
 * A successful response of any method, as sent on the wire.
 */
export type Reply = { "method": "node.info", "result": NodeInfo } | { "method": "harness.list", "result": Array<Harness> } | { "method": "session.list", "result": Array<SessionInfo> } | { "method": "session.get", "result": SessionInfo } | { "method": "session.create", "result": SessionInfo } | { "method": "session.send", "result": Ack } | { "method": "session.kill", "result": Ack } | { "method": "session.read", "result": EventPage } | { "method": "session.subscribe", "result": Subscribed } | { "method": "session.unsubscribe", "result": Ack } | { "method": "node.stop", "result": Ack } | { "method": "device.invite", "result": Invitation } | { "method": "device.list", "result": Array<DeviceInfo> } | { "method": "device.revoke", "result": Ack } | { "method": "profile.list", "result": Array<ProfileInfo> } | { "method": "profile.reload", "result": Array<ProfileInfo> } | { "method": "session.move_profile", "result": SessionInfo } | { "method": "fleet.manifest", "result": Manifest } | { "method": "fleet.blob", "result": BlobChunk } | { "method": "fleet.watch", "result": Generation } | { "method": "fleet.report", "result": Ack } | { "method": "fleet.members", "result": Array<FleetMember> } | { "method": "fleet.forward", "result": Forwarded } | { "method": "fleet.relay", "result": Forwarded } | { "method": "fleet.deliver", "result": Forwarded } | { "method": "fleet.grant", "result": Ack } | { "method": "fleet.callback", "result": Callback } | { "method": "fleet.peers", "result": Array<FleetNode> } | { "method": "fleet.nodes", "result": Array<FleetNode> } | { "method": "fleet.forget", "result": NodeForgotten } | { "method": "session.export", "result": TransferManifest } | { "method": "session.import", "result": SessionImported } | { "method": "memory.export", "result": TransferManifest } | { "method": "memory.import", "result": MemoryImported } | { "method": "transfer.read", "result": BlobChunk } | { "method": "transfer.close", "result": Ack } | { "method": "secret.set", "result": Ack } | { "method": "secret.list", "result": Array<SecretName> } | { "method": "secret.delete", "result": Ack } | { "method": "secret.copy", "result": Ack } | { "method": "model.list", "result": Array<ModelRoute> };

/**
 * `fleet.report` (protocol 5): a member says what it applied and which
 * files it left alone because they were edited locally. Needs `Sync`.
 */
export type ReportApplied = { 
/**
 * The generation applied.
 */
generation: Generation, 
/**
 * Files kept as edited, with the incoming version written beside them.
 */
conflicts?: Array<BundlePath>, };

/**
 * `device.revoke` (protocol 4): revoke a device and close its connections.
 */
export type RevokeDevice = { 
/**
 * Which device.
 */
device: DeviceId, };

/**
 * A route to a node: a `ws://` or `wss://` URL, for a relay or a direct
 * address. Only the scheme is checked here; reachability is the client's.
 */
export type RouteUrl = string;

/**
 * What a caller may do. Every RPC method declares the one it needs
 * (`Method::scope`); the node checks it for calls from a paired device.
 * The node token and the local socket hold every scope.
 */
export type Scope = "read" | "operate" | "admin" | "sync" | "relay";

/**
 * A secret's name in a node's `secrets.toml`; definitions refer to
 * secrets by it, so the value never leaves the node it is on.
 */
export type SecretName = string;

/**
 * A secret's value, on its way into a node's store.
 */
export type SecretValue = string;

/**
 * `session.send`: send input to a live session.
 */
export type SendInput = { 
/**
 * Which session.
 */
session: SessionId, 
/**
 * What to send.
 */
input: SessionInput, };

/**
 * Something that happened in a session.
 */
export type SessionEvent = { "type": "status", 
/**
 * New state.
 */
status: SessionStatus, } | { "type": "terminal" } & TerminalOutput | { "type": "agent", 
/**
 * The entry.
 */
event: AgentEvent, } | { "type": "profile_changed", 
/**
 * The profile before; `None` is the harness's default.
 */
from?: ProfileName | null, 
/**
 * The profile now.
 */
to?: ProfileName | null, } | { "type": "model_call" } & ModelCall;

/**
 * A session on a node. Unique across the fleet, so it can be used
 * without the node id once peers exist.
 */
export type SessionId = string;

/**
 * Reply to `session.import`: where the conversation is now.
 */
export type SessionImported = { 
/**
 * The harness's id for it, to resume it by later.
 */
harness_session: HarnessSessionId, 
/**
 * The profile it was placed in; absent is the default.
 */
profile?: ProfileName | null, 
/**
 * The working directory it belongs to here.
 */
cwd: string, 
/**
 * Files written.
 */
written: number, 
/**
 * The new session, when `start` asked for one.
 */
session?: SessionInfo | null, };

/**
 * A session as reported by the node.
 *
 * `#[non_exhaustive]` for the reason given on [`SessionSpec`]; build one
 * with [`SessionInfo::new`].
 */
export type SessionInfo = { 
/**
 * Session id.
 */
id: SessionId, 
/**
 * What was asked for.
 */
spec: SessionSpec, 
/**
 * The profile the harness runs with now: `spec.profile` until
 * `session.move_profile` changes it (protocol 5).
 */
profile?: ProfileName | null, 
/**
 * Who created it (protocol 5, ADR 0013). Absent from older nodes, which
 * only ever had the owner.
 */
created_by: Creator, 
/**
 * Current process state.
 */
status: SessionStatus, 
/**
 * When the session was created.
 */
created_at: string, 
/**
 * When the process ended, if it has.
 */
ended_at?: string | null, 
/**
 * Sequence number the next event will get (= number of events so far).
 */
next_seq: EventSeq, 
/**
 * The harness's own session id, once a structured session has one.
 */
harness_session?: HarnessSessionId | null, 
/**
 * The home a call creating it came through, when another node asked
 * for it (protocol 6).
 */
via?: NodeId | null, 
/**
 * Where it came from, when not from this node (protocol 6).
 */
origin?: SessionOrigin | null, 
/**
 * The model route it was launched with, and the provider's model its
 * harness is pinned to (protocol 7). Absent for a session on its
 * harness's own provider, and from older nodes.
 */
model?: SessionModel | null, };

/**
 * Something a client sends into a live session. Body of `session.send`.
 *
 * Sending a variant that does not match the session's mode fails with
 * `ErrorCode::Unsupported`; the node never guesses a conversion.
 */
export type SessionInput = { "type": "terminal", 
/**
 * The input.
 */
input: TerminalInput, } | { "type": "prompt", 
/**
 * The user's message.
 */
content: Array<ContentBlock>, } | { "type": "cancel_turn" } | { "type": "answer_permission", 
/**
 * Which request.
 */
id: PermissionId, 
/**
 * The choice.
 */
outcome: PermissionOutcome, };

/**
 * The route a session was launched with (protocol 7).
 */
export type SessionModel = { 
/**
 * The route.
 */
route: ModelRouteName, 
/**
 * The provider's model the harness is pinned to.
 */
model: ModelId, };

/**
 * Where a session came from, when another node of the fleet had a hand in
 * it (protocol 6). `created_by` says who created it on this node; an
 * agent's own node is in `created_by` already, so `remote` names only the
 * owner or a device of the other node.
 */
export type SessionOrigin = { "kind": "remote", 
/**
 * The node the call started on.
 */
node: NodeId, 
/**
 * Who started it there.
 */
by: RemoteCreator, } | { "kind": "imported", 
/**
 * The node it was copied from.
 */
node: NodeId, 
/**
 * The session there.
 */
session: SessionId, };

/**
 * A session on a node of the fleet.
 */
export type SessionRef = { 
/**
 * The node.
 */
node: NodeId, 
/**
 * The session there.
 */
session: SessionId, };

/**
 * What to run. Body of `session.create`.
 *
 * `#[non_exhaustive]` with a constructor (protocol 5): every field added
 * since protocol 2 broke each struct literal in the workspace. Build one
 * with [`SessionSpec::new`] and the `with_*` methods; fields stay public
 * for reading and for changing after construction.
 */
export type SessionSpec = { 
/**
 * Harness to launch.
 */
harness: HarnessKind, 
/**
 * Working directory. Defaults to the node's home directory.
 */
cwd?: string | null, 
/**
 * The profile to launch with (protocol 5, ADR 0011); `None` is the
 * harness's default, which is how every earlier session ran.
 */
profile?: ProfileName | null, } & ({ "mode": "terminal", 
/**
 * Extra arguments passed verbatim to the harness binary.
 */
args: Array<string>, 
/**
 * Initial window size.
 */
size: TerminalSize, } | { "mode": "structured", 
/**
 * Model to request, if not the harness default.
 */
model?: ModelId | null, 
/**
 * Resume this harness session instead of starting a new one.
 */
resume?: HarnessSessionId | null, });

/**
 * Lifecycle of the harness process behind a session.
 *
 * This is process state only. Whether a structured session is mid-turn is
 * derived from `TurnStarted`/`TurnEnded` events, so there is one source of
 * truth for each fact.
 */
export type SessionStatus = { "state": "starting" } | { "state": "running" } | { "state": "exited", 
/**
 * Exit code, if the OS reported one (none when killed by a signal).
 */
code: number | null, } | { "state": "failed", 
/**
 * Human-readable reason.
 */
message: string, } | { "state": "interrupted" };

/**
 * `secret.set` (protocol 6): store a value in this node's `secrets.toml`,
 * replacing one of the same name. Needs `admin`.
 */
export type SetSecret = { 
/**
 * Its name.
 */
name: SecretName, 
/**
 * Its value.
 */
value: SecretValue, };

/**
 * `node.stop` (protocol 3): stop the node, now or once idle. Used by
 * remote update; a client that negotiated protocol 2 never sends it.
 */
export type StopNode = { 
/**
 * When to stop.
 */
when: StopWhen, };

/**
 * Why a turn stopped.
 */
export type StopReason = "end_turn" | "max_tokens" | "max_turn_requests" | "refusal" | "cancelled";

/**
 * When `node.stop` takes effect.
 */
export type StopWhen = "now" | "when_idle";

/**
 * Whether a harness can run as a structured (ACP) session on this node.
 */
export type StructuredSupport = { "state": "available" } | { "state": "unavailable", 
/**
 * Human-readable reason.
 */
reason: string, };

/**
 * `session.subscribe`: stream a session's events from `from` onwards.
 */
export type Subscribe = { 
/**
 * Which session.
 */
session: SessionId, 
/**
 * First sequence number wanted (zero for the whole history).
 */
from: EventSeq, };

/**
 * Reply to `session.subscribe`. Events follow as `ServerFrame::Event`.
 */
export type Subscribed = { 
/**
 * Id that tags the events of this subscription.
 */
subscription: SubscriptionId, 
/**
 * The session at the moment of subscribing.
 */
info: SessionInfo, };

/**
 * Why a subscription stopped delivering events.
 */
export type SubscriptionEnd = { "reason": "session_ended" } | { "reason": "unsubscribed" } | { "reason": "lagged", 
/**
 * First sequence number not delivered.
 */
resume_from: EventSeq, };

/**
 * Node-assigned id of one event subscription; unique per connection.
 */
export type SubscriptionId = number;

/**
 * Raw bytes to or from a PTY.
 *
 * [Decision Log]
 * - Purpose: PTY output is arbitrary bytes. A read can end in the middle of
 *   a UTF-8 sequence or an escape sequence, so it cannot be a `String`.
 * - Alternatives: a JSON array of numbers (4x size); lossy UTF-8 (corrupts
 *   split characters); separate binary websocket frames (a second framing
 *   scheme next to the typed JSON frames, and harder to relay and to log).
 * - Choice: `Bytes` in memory (cheap to fan out), base64 text on the wire
 *   and in the session log.
 * - Cost: about 33% size overhead on terminal traffic. Revisit with a binary
 *   codec if terminal bandwidth ever matters; the type does not change.
 */
export type TerminalBytes = string;

/**
 * Input to a terminal-mode session.
 */
export type TerminalInput = { "kind": "data", 
/**
 * The bytes.
 */
data: TerminalBytes, } | { "kind": "resize", 
/**
 * New size.
 */
size: TerminalSize, };

/**
 * A chunk of PTY output.
 */
export type TerminalOutput = { 
/**
 * The bytes, as read from the PTY.
 */
data: TerminalBytes, };

/**
 * Terminal window size in character cells.
 */
export type TerminalSize = { 
/**
 * Columns.
 */
cols: number, 
/**
 * Rows.
 */
rows: number, };

/**
 * A ticket's HMAC-SHA256 under the relay's token (`vessel_relay`
 * computes and checks it). Not printed: it is no use without the
 * ticket's node key, but nothing needs to see it.
 */
export type TicketMac = string;

/**
 * A number of tokens.
 */
export type TokenCount = number;

/**
 * The tokens one call used.
 */
export type TokenUsage = { 
/**
 * Input tokens neither read from nor written to a prompt cache.
 */
input: TokenCount, 
/**
 * Input tokens read from a prompt cache.
 */
cache_read: TokenCount, 
/**
 * Input tokens written to a prompt cache.
 */
cache_write: TokenCount, 
/**
 * Output tokens, reasoning included.
 */
output: TokenCount, 
/**
 * Of the output, the tokens spent reasoning, where the provider says.
 */
reasoning: TokenCount, };

/**
 * A tool call as first announced.
 */
export type ToolCall = { 
/**
 * Harness-assigned id.
 */
id: ToolCallId, 
/**
 * Short human-readable description.
 */
title: string, 
/**
 * Category.
 */
kind: ToolKind, 
/**
 * Initial status.
 */
status: ToolCallStatus, 
/**
 * Content produced so far.
 */
content: Array<ToolCallContent>, 
/**
 * Files involved.
 */
locations: Array<ToolLocation>, 
/**
 * The tool's raw input arguments, forwarded and never interpreted.
 */
raw_input?: RawJson | null, };

/**
 * Output attached to a tool call.
 */
export type ToolCallContent = { "type": "content", 
/**
 * The content.
 */
content: ContentBlock, } | { "type": "diff", 
/**
 * File being changed.
 */
path: string, 
/**
 * Previous text (`None` for a new file).
 */
old_text?: string | null, 
/**
 * New text.
 */
new_text: string, };

/**
 * A tool call id assigned by the harness.
 */
export type ToolCallId = string;

/**
 * Progress of a tool call.
 */
export type ToolCallStatus = "pending" | "in_progress" | "completed" | "failed";

/**
 * A change to an announced tool call. `None` fields are unchanged; `Some`
 * collections replace the previous value.
 */
export type ToolCallUpdate = { 
/**
 * Which call.
 */
id: ToolCallId, 
/**
 * New title.
 */
title?: string | null, 
/**
 * New category.
 */
kind?: ToolKind | null, 
/**
 * New status.
 */
status?: ToolCallStatus | null, 
/**
 * Replacement content.
 */
content?: Array<ToolCallContent> | null, 
/**
 * Replacement locations.
 */
locations?: Array<ToolLocation> | null, 
/**
 * The tool's raw input arguments, when the harness sends them only with
 * an update (Claude's adapter often does). Forwarded, never interpreted.
 */
raw_input?: RawJson | null, 
/**
 * The tool's raw output, forwarded and never interpreted.
 */
raw_output?: RawJson | null, };

/**
 * What kind of work a tool call does. Lets clients pick an icon and a
 * default rendering without knowing harness tool names.
 */
export type ToolKind = "read" | "edit" | "delete" | "move" | "search" | "execute" | "think" | "fetch" | "switch_mode" | "other";

/**
 * A file location a tool call touches.
 */
export type ToolLocation = { 
/**
 * Path on the node.
 */
path: string, 
/**
 * 1-based line, if known.
 */
line?: number | null, };

/**
 * What a transfer holds.
 */
export type TransferDetail = { "kind": "session", 
/**
 * The harness's id for the conversation, to resume it by.
 */
harness_session: HarnessSessionId, 
/**
 * The directory it ran in (the files are named after it).
 */
cwd: string, 
/**
 * The session as its node reports it: harness, profile, creator.
 */
info: SessionInfo, } | { "kind": "memory", 
/**
 * Which harness.
 */
harness: HarnessKind, 
/**
 * Whose memory; absent is the default profile.
 */
profile?: ProfileName | null, 
/**
 * The project it is for, if only one.
 */
project?: string | null, };

/**
 * An open transfer: a snapshot of files a node holds for whoever opened
 * it (protocol 6). Random, but not a secret: the node also checks that
 * the caller reading or closing it is the one that opened it.
 */
export type TransferId = string;

/**
 * Reply to `session.export` and `memory.export`.
 */
export type TransferManifest = { 
/**
 * The open transfer.
 */
transfer: TransferId, 
/**
 * Every file, by its path under the harness home.
 */
files: { [key in BundlePath]: BundleFile }, 
/**
 * What the files are.
 */
detail: TransferDetail, };

/**
 * `session.unsubscribe`: stop a subscription on this connection.
 */
export type Unsubscribe = { 
/**
 * Which subscription.
 */
subscription: SubscriptionId, };

/**
 * `fleet.watch` (protocol 5): wait for a generation newer than `after`.
 * A long poll, not a subscription, so it needs no new frame type: the home
 * answers as soon as one is published, or with its current generation
 * (possibly `after` itself) when its wait limit passes, and the member asks
 * again. Needs `Sync`.
 */
export type WatchFleet = { 
/**
 * The generation the member has.
 */
after: Generation, };

/**
 * An error returned in place of a response.
 */
export type WireError = { 
/**
 * Machine-readable kind.
 */
code: ErrorCode, 
/**
 * Human-readable explanation. Never contains secrets.
 */
message: string, };
