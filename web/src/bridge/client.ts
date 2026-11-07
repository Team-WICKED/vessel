// The page's side of the bridge (ADR 0017): one websocket to `vessel ui`,
// typed calls, followed sessions, node states.
//
// [Decision Log]
// - Purpose: every screen talks to nodes through this one socket. It
//   must survive the socket dropping (a laptop sleeping) without losing
//   or repeating a session's records, and never repeat a call that may
//   have run.
// - Choice: calls are promises keyed by id; when the socket drops, every
//   call in flight fails as `disconnected` (the node may have run it, so
//   nothing retries it). Streams remember the next record they need and
//   are followed again from it after each hello; the bridge's own streams
//   are gap-free, so the stores just continue. Node states are kept as
//   the bridge last pushed them, and pushed again in full after a hello.
// - The socket redials with a capped backoff, except after the bridge
//   refused the token (close 4401): a new launch has a new token, and
//   only opening the new address helps.

import { Handshake } from "./auth";
import { rememberSession, type Credential } from "./token";
import type { MethodName, ParamsOf, ResultOf } from "../wire/calls";
import type {
  ClientFrame,
  Failure,
  NodeKey,
  NodeStatus,
  Op,
  OpResult,
  ServerFrame,
  StreamState,
} from "../wire/bridge";
import type { Call, EventRecord, Reply, SessionId } from "../wire/proto";

/** Where the page's link to `vessel ui` is. */
export type LinkState =
  | { kind: "connecting"; attempt: number }
  | { kind: "open"; version: string }
  | { kind: "closed"; retryInMs: number; attempt: number }
  | { kind: "refused" };

/** A call that has no reply, with the bridge's reason. */
export class CallFailed extends Error {
  readonly failure: Failure;

  constructor(failure: Failure) {
    super(describeFailure(failure));
    this.name = "CallFailed";
    this.failure = failure;
  }
}

/** A failure as one line for the operator. */
export function describeFailure(failure: Failure): string {
  switch (failure.kind) {
    case "remote":
      return failure.error.message;
    case "not_in_protocol":
      return `${failure.method} needs protocol ${failure.needs}; the node speaks ${failure.negotiated}`;
    case "disconnected":
      return "The connection closed before the node answered; it may or may not have run.";
    case "unreachable":
    case "refused":
    case "failed":
      return failure.message;
  }
}

/** What a followed session's stream reports. */
export interface StreamHandlers {
  record: (record: EventRecord) => void;
  state: (state: StreamState) => void;
}

interface Pending {
  resolve: (reply: Reply) => void;
  reject: (error: CallFailed) => void;
}

interface PendingOp {
  resolve: (result: OpResult) => void;
  reject: (error: Error) => void;
}

interface Followed {
  node: NodeKey;
  session: SessionId;
  /** The next record wanted. */
  next: number;
  handlers: StreamHandlers;
}

const CLOSE_UNAUTHORIZED = 4401;
const FIRST_RETRY_MS = 500;
const MAX_RETRY_MS = 10_000;
const MAX_RETRIES = 5;

/** The websocket address of the page's own server. */
function socketUrl(): string {
  const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${window.location.host}/api/ws`;
}

export class Bridge {
  #credential: Credential;
  readonly #url: string;
  #socket: WebSocket | null = null;
  #nextId = 1;
  #attempt = 0;
  #stopped = false;
  #retry: ReturnType<typeof setTimeout> | null = null;
  readonly #calls = new Map<number, Pending>();
  readonly #ops = new Map<number, PendingOp>();
  readonly #streams = new Map<number, Followed>();
  readonly #statuses = new Map<NodeKey, NodeStatus>();
  readonly #nodeListeners = new Set<
    (node: NodeKey, status: NodeStatus) => void
  >();
  readonly #linkListeners = new Set<(link: LinkState) => void>();
  #link: LinkState = { kind: "connecting", attempt: 0 };

  constructor(token: Credential, url: string = socketUrl()) {
    this.#credential = token;
    this.#url = url;
  }

  get link(): LinkState {
    return this.#link;
  }

  /** Open the socket (and keep it open). */
  start(): void {
    this.#stopped = false;
    this.#connect();
  }

  /** Close for good. */
  stop(): void {
    this.#stopped = true;
    if (this.#retry !== null) {
      clearTimeout(this.#retry);
    }
    this.#socket?.close();
    this.#socket = null;
  }

  /** Call `method` on `node`. Rejects with `CallFailed`. */
  async call<M extends MethodName>(
    node: NodeKey,
    method: M,
    params: ParamsOf<M>,
  ): Promise<ResultOf<M>> {
    const id = this.#id();
    // The pair is a member of the `Call` union by construction; TypeScript
    // cannot see that through the generic.
    const call = { method, params } as unknown as Call;
    const reply = await new Promise<Reply>((resolve, reject) => {
      this.#calls.set(id, { resolve, reject });
      if (!this.#send({ type: "call", id, node, call })) {
        this.#calls.delete(id);
        reject(
          new CallFailed({
            kind: "unreachable",
            message: "Not connected to vessel ui.",
          }),
        );
      }
    });
    if (reply.method !== method) {
      throw new CallFailed({
        kind: "failed",
        message: `The bridge answered ${reply.method} to ${method}.`,
      });
    }
    return reply.result as ResultOf<M>;
  }

  /** Ask the bridge itself. Rejects with an `Error` holding its message. */
  op(op: Op): Promise<OpResult> {
    const id = this.#id();
    return new Promise<OpResult>((resolve, reject) => {
      this.#ops.set(id, { resolve, reject });
      if (!this.#send({ type: "op", id, op })) {
        this.#ops.delete(id);
        reject(new Error("Not connected to vessel ui."));
      }
    });
  }

  /** Follow `session` from record `from`; the returned function stops. */
  follow(
    node: NodeKey,
    session: SessionId,
    from: number,
    handlers: StreamHandlers,
  ): () => void {
    const stream = this.#id();
    this.#streams.set(stream, { node, session, next: from, handlers });
    this.#send({ type: "follow", stream, node, session, from });
    return () => {
      if (this.#streams.delete(stream)) {
        this.#send({ type: "unfollow", stream });
      }
    };
  }

  /** Every node's last known state. */
  statuses(): ReadonlyMap<NodeKey, NodeStatus> {
    return this.#statuses;
  }

  onNode(listener: (node: NodeKey, status: NodeStatus) => void): () => void {
    this.#nodeListeners.add(listener);
    return () => this.#nodeListeners.delete(listener);
  }

  onLink(listener: (link: LinkState) => void): () => void {
    this.#linkListeners.add(listener);
    return () => this.#linkListeners.delete(listener);
  }

  #id(): number {
    const id = this.#nextId;
    this.#nextId += 1;
    return id;
  }

  #setLink(link: LinkState): void {
    this.#link = link;
    for (const listener of this.#linkListeners) {
      listener(link);
    }
  }

  #send(frame: ClientFrame): boolean {
    const socket = this.#socket;
    if (socket?.readyState !== WebSocket.OPEN) {
      return false;
    }
    // Before the welcome only the hello may go.
    if (frame.type !== "hello" && this.#link.kind !== "open") {
      return false;
    }
    socket.send(JSON.stringify(frame));
    return true;
  }

  #connect(): void {
    this.#setLink({ kind: "connecting", attempt: this.#attempt });
    const socket = new WebSocket(this.#url);
    this.#socket = socket;
    const handshake = new Handshake(this.#credential);
    let verified = false;
    let messages = Promise.resolve();
    const deadline = setTimeout(() => {
      if (this.#socket === socket && !verified) this.#refuse();
    }, 10_000);
    socket.addEventListener("message", (event: MessageEvent<unknown>) => {
      if (!this.#current(socket)) return;
      const data = event.data;
      messages = messages
        .then(async () => {
          if (!this.#current(socket)) return;
          if (typeof data !== "string") throw new Error("Not a frame");
          const frame = JSON.parse(data) as ServerFrame;
          if (!verified) {
            if (frame.type === "challenge") {
              const hello = await handshake.hello(frame);
              if (this.#current(socket)) this.#send(hello);
            } else if (frame.type === "welcome") {
              const credential = await handshake.welcome(frame);
              if (!this.#current(socket)) return;
              this.#credential = credential;
              rememberSession(credential);
              verified = true;
              clearTimeout(deadline);
              this.#receive(frame);
            } else {
              throw new Error("Server has not proved its identity");
            }
          } else {
            if (frame.type === "challenge" || frame.type === "welcome")
              throw new Error("Repeated handshake");
            this.#receive(frame);
          }
        })
        .catch(() => {
          if (this.#socket === socket) this.#refuse();
        });
    });
    socket.addEventListener("close", (event) => {
      clearTimeout(deadline);
      if (this.#socket !== socket) {
        return;
      }
      this.#socket = null;
      this.#lost();
      if (event.code === CLOSE_UNAUTHORIZED) {
        this.#setLink({ kind: "refused" });
        return;
      }
      if (!this.#stopped) {
        this.#attempt += 1;
        // A restarted process rejects this launch's old hostname at HTTP
        // upgrade, where browsers hide the response status. Bound those
        // failures too, instead of reconnecting an obsolete tab forever.
        if (this.#attempt >= MAX_RETRIES) {
          this.#refuse();
          return;
        }
        const retryInMs = Math.min(
          FIRST_RETRY_MS * 2 ** (this.#attempt - 1),
          MAX_RETRY_MS,
        );
        this.#setLink({ kind: "closed", retryInMs, attempt: this.#attempt });
        this.#retry = setTimeout(() => {
          this.#connect();
        }, retryInMs);
      }
    });
  }

  #current(socket: WebSocket): boolean {
    return this.#socket === socket && !this.#stopped;
  }

  #refuse(): void {
    this.stop();
    this.#lost();
    this.#setLink({ kind: "refused" });
  }

  /** The socket went: calls in flight fail, never retried. */
  #lost(): void {
    for (const pending of this.#calls.values()) {
      pending.reject(new CallFailed({ kind: "disconnected" }));
    }
    this.#calls.clear();
    for (const pending of this.#ops.values()) {
      pending.reject(new Error("The connection to vessel ui closed."));
    }
    this.#ops.clear();
  }

  #receive(frame: ServerFrame): void {
    switch (frame.type) {
      case "challenge":
        return;
      case "welcome":
        this.#attempt = 0;
        this.#setLink({ kind: "open", version: frame.version });
        // Every stream continues where it was.
        for (const [stream, followed] of this.#streams) {
          this.#send({
            type: "follow",
            stream,
            node: followed.node,
            session: followed.session,
            from: followed.next,
          });
        }
        return;
      case "reply": {
        const pending = this.#calls.get(frame.id);
        this.#calls.delete(frame.id);
        if (pending === undefined) {
          return;
        }
        if ("ok" in frame.outcome) {
          pending.resolve(frame.outcome.ok);
        } else {
          pending.reject(new CallFailed(frame.outcome.err));
        }
        return;
      }
      case "op_reply": {
        const pending = this.#ops.get(frame.id);
        this.#ops.delete(frame.id);
        if (pending === undefined) {
          return;
        }
        if ("ok" in frame.outcome) {
          pending.resolve(frame.outcome.ok);
        } else {
          pending.reject(new Error(frame.outcome.err.message));
        }
        return;
      }
      case "node":
        this.#statuses.set(frame.node, frame.status);
        for (const listener of this.#nodeListeners) {
          listener(frame.node, frame.status);
        }
        return;
      case "event": {
        const followed = this.#streams.get(frame.stream);
        if (followed === undefined || frame.record.seq < followed.next) {
          return;
        }
        followed.next = frame.record.seq + 1;
        followed.handlers.record(frame.record);
        return;
      }
      case "stream": {
        const followed = this.#streams.get(frame.stream);
        if (followed === undefined) {
          return;
        }
        if (frame.state.state === "missing") {
          followed.next = frame.state.resumed_at;
        }
        if (frame.state.state === "ended" || frame.state.state === "failed") {
          this.#streams.delete(frame.stream);
        }
        followed.handlers.state(frame.state);
        return;
      }
      case "unreadable":
        console.warn(`vessel ui could not read a frame: ${frame.message}`);
        return;
    }
  }
}
