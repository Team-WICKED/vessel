// The fleet as the page knows it: the book's nodes, each node's link
// state (pushed by the bridge), and what each connected node reports
// (harnesses, profiles, model routes, sessions).
//
// Sessions have no list subscription in the protocol, so each connected
// node's list is read again every few seconds while the page is visible;
// rows patch in place because unchanged sessions keep their objects.

import { Bridge, CallFailed, type LinkState } from "../bridge/client";
import type { BookNode, NodeKey, NodeStatus } from "../wire/bridge";
import type {
  Harness,
  ModelRoute,
  ProfileInfo,
  SessionInfo,
} from "../wire/proto";
import { Store } from "./store";

const SESSIONS_EVERY_MS = 2000;

export interface FleetState {
  link: LinkState;
  /** The nodes to list; `null` until the bridge has answered. */
  book: readonly BookNode[] | null;
  bookError: string | null;
  statuses: Readonly<Record<NodeKey, NodeStatus>>;
  harnesses: Readonly<Record<NodeKey, readonly Harness[]>>;
  profiles: Readonly<Record<NodeKey, readonly ProfileInfo[]>>;
  /** The model routes each node offers (`model.list`, protocol 7);
   * `null` for a node older than model routing. */
  models: Readonly<Record<NodeKey, readonly ModelRoute[] | null>>;
  sessions: Readonly<Record<NodeKey, readonly SessionInfo[]>>;
  /** The last error reading a node, by node. */
  errors: Readonly<Record<NodeKey, string>>;
  /** The node the operator last chose (DESIGN.md §2.5). */
  selected: NodeKey | null;
}

const INITIAL: FleetState = {
  link: { kind: "connecting", attempt: 0 },
  book: null,
  bookError: null,
  statuses: {},
  harnesses: {},
  profiles: {},
  models: {},
  sessions: {},
  errors: {},
  selected: null,
};

/** `next` with the objects of `previous` kept where nothing changed, so
 * React re-renders only the rows that did. */
function keep(
  previous: readonly SessionInfo[] | undefined,
  next: SessionInfo[],
): SessionInfo[] {
  if (previous === undefined) {
    return next;
  }
  const byId = new Map(previous.map((session) => [session.id, session]));
  return next.map((session) => {
    const old = byId.get(session.id);
    return old !== undefined && JSON.stringify(old) === JSON.stringify(session)
      ? old
      : session;
  });
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export class Fleet {
  readonly bridge: Bridge;
  readonly store = new Store<FleetState>(INITIAL);
  #timer: ReturnType<typeof setInterval> | null = null;

  constructor(bridge: Bridge) {
    this.bridge = bridge;
    bridge.onLink((link) => {
      this.store.update((state) => ({ ...state, link }));
      if (link.kind === "open") {
        void this.loadBook();
      }
    });
    bridge.onNode((node, status) => {
      const before = this.store.get().statuses[node];
      this.store.update((state) => ({
        ...state,
        statuses: { ...state.statuses, [node]: status },
      }));
      if (status.state === "connected" && before?.state !== "connected") {
        void this.refresh(node);
      }
    });
  }

  start(): void {
    this.bridge.start();
    this.#timer = setInterval(() => {
      if (document.visibilityState === "visible") {
        this.#pollSessions();
      }
    }, SESSIONS_EVERY_MS);
  }

  stop(): void {
    if (this.#timer !== null) {
      clearInterval(this.#timer);
    }
    this.bridge.stop();
  }

  select(node: NodeKey | null): void {
    this.store.update((state) =>
      state.selected === node ? state : { ...state, selected: node },
    );
  }

  /** The book's nodes, then a first read of each (which connects it). */
  async loadBook(): Promise<void> {
    try {
      const result = await this.bridge.op({ op: "nodes" });
      if (result.op !== "nodes") {
        return;
      }
      this.store.update((state) => ({
        ...state,
        book: result.nodes,
        bookError: null,
      }));
      await Promise.all(result.nodes.map((node) => this.refresh(node.key)));
    } catch (error) {
      this.store.update((state) => ({ ...state, bookError: message(error) }));
    }
  }

  /** Read what `node` reports. */
  async refresh(node: NodeKey): Promise<void> {
    try {
      const [harnesses, profiles, models, sessions] = await Promise.all([
        this.bridge.call(node, "harness.list", null),
        this.bridge.call(node, "profile.list", {}),
        this.#models(node),
        this.bridge.call(node, "session.list", null),
      ]);
      this.store.update((state) => {
        const errors = Object.fromEntries(
          Object.entries(state.errors).filter(([key]) => key !== node),
        );
        return {
          ...state,
          harnesses: { ...state.harnesses, [node]: harnesses },
          profiles: { ...state.profiles, [node]: profiles },
          models: { ...state.models, [node]: models },
          sessions: {
            ...state.sessions,
            [node]: keep(state.sessions[node], sessions),
          },
          errors,
        };
      });
    } catch (error) {
      this.store.update((state) => ({
        ...state,
        errors: { ...state.errors, [node]: message(error) },
      }));
    }
  }

  /** `node`'s routes; `null` when it predates them, so the rest of what
   * it reports still shows. */
  async #models(node: NodeKey): Promise<readonly ModelRoute[] | null> {
    try {
      return await this.bridge.call(node, "model.list", null);
    } catch (error) {
      if (
        error instanceof CallFailed &&
        error.failure.kind === "not_in_protocol"
      ) {
        return null;
      }
      throw error;
    }
  }

  async refreshSessions(node: NodeKey): Promise<void> {
    try {
      const sessions = await this.bridge.call(node, "session.list", null);
      this.store.update((state) => ({
        ...state,
        sessions: {
          ...state.sessions,
          [node]: keep(state.sessions[node], sessions),
        },
      }));
    } catch {
      // The node's state says why; the list stays as it was.
    }
  }

  #pollSessions(): void {
    for (const [node, status] of Object.entries(this.store.get().statuses)) {
      if (status.state === "connected") {
        void this.refreshSessions(node);
      }
    }
  }
}
