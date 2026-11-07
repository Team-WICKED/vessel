// A structured session's timeline: its records, folded into entries with
// stable keys (DESIGN.md §6.5; ADR 0017).
//
// [Decision Log]
// - Purpose: the session view renders a log, not the raw records: chunks
//   of one message merged, a tool call updated in place, a permission
//   request with its answer, the latest plan.
// - Choice: a pure reducer over `EventRecord`s in sequence order, fed in
//   batches. It copies the entry list once per batch and replaces only the
//   entries that change, so React re-renders those alone, and replaying a
//   long log is linear. Keys come from sequence numbers and the protocol's
//   ids, so an entry keeps its key while it grows.
// - Model calls (protocol 7) are entries too, and summed into `usage` as
//   they arrive, so the inspector's totals cost nothing per render. They
//   carry metadata only; the record holds no content to show.
// - It is plain TypeScript with no React, tested on its own, including
//   against records a real node produced with `fake-acp`.

import type {
  AgentEvent,
  ContentBlock,
  EventRecord,
  ModelCall,
  PermissionOutcome,
  PermissionRequest,
  PlanEntry,
  SessionStatus,
  StopReason,
  ToolCallContent,
  ToolCallStatus,
  ToolKind,
  ToolLocation,
} from "../wire/proto";
import { addCall, type Usage } from "./models";

/** A tool call as it stands after its updates. */
export interface ToolState {
  id: string;
  title: string;
  kind: ToolKind;
  status: ToolCallStatus;
  content: ToolCallContent[];
  locations: ToolLocation[];
  input: unknown;
  output: unknown;
}

interface Base {
  key: string;
  seq: number;
  at: string;
}

export type Entry =
  | (Base & { kind: "ready"; harnessSession: string })
  | (Base & { kind: "prompt"; text: string; replayed: boolean; turn: number })
  | (Base & { kind: "message"; text: string })
  | (Base & { kind: "thought"; text: string })
  | (Base & { kind: "tool"; tool: ToolState })
  | (Base & { kind: "plan"; entries: PlanEntry[] })
  | (Base & {
      kind: "permission";
      request: PermissionRequest;
      title: string;
      outcome: PermissionOutcome | null;
    })
  | (Base & { kind: "turn_ended"; reason: StopReason })
  | (Base & { kind: "turn_failed"; message: string })
  | (Base & { kind: "log"; line: string })
  | (Base & { kind: "status"; status: SessionStatus })
  | (Base & { kind: "profile"; from: string | null; to: string | null })
  | (Base & { kind: "model"; call: ModelCall });

export interface Timeline {
  entries: readonly Entry[];
  /** The latest plan. */
  plan: readonly PlanEntry[] | null;
  /** Turns started so far. */
  turns: number;
  /** Whether a turn is running. */
  inTurn: boolean;
  /** The first permission request still unanswered. */
  pending: Extract<Entry, { kind: "permission" }> | null;
  /** Paths the session's tool calls touched, in first-touched order. */
  files: readonly string[];
  /** The session's status, as its records last said. */
  status: SessionStatus | null;
  /** Its model calls so far; `null` before the first. */
  usage: Usage | null;
  /** The next record expected. */
  next: number;
}

export const EMPTY: Timeline = {
  entries: [],
  plan: null,
  turns: 0,
  inTurn: false,
  pending: null,
  files: [],
  status: null,
  usage: null,
  next: 0,
};

/** A content block as text. */
export function blockText(block: ContentBlock): string {
  switch (block.type) {
    case "text":
      return block.text;
    case "resource_link":
      return `[${block.name}](${block.uri})`;
    case "opaque":
      return "";
  }
}

/** `timeline` with `records` applied (records already seen are skipped). */
export function reduce(
  timeline: Timeline,
  records: readonly EventRecord[],
): Timeline {
  const fresh = records.filter((record) => record.seq >= timeline.next);
  if (fresh.length === 0) {
    return timeline;
  }
  const work = new Work(timeline);
  for (const record of fresh) {
    work.apply(record);
  }
  return work.finish();
}

/** One batch's mutable copy. */
class Work {
  readonly entries: Entry[];
  plan: readonly PlanEntry[] | null;
  turns: number;
  inTurn: boolean;
  readonly files: string[];
  status: SessionStatus | null;
  usage: Usage | null;
  next: number;

  constructor(timeline: Timeline) {
    this.entries = [...timeline.entries];
    this.plan = timeline.plan;
    this.turns = timeline.turns;
    this.inTurn = timeline.inTurn;
    this.files = [...timeline.files];
    this.status = timeline.status;
    this.usage = timeline.usage;
    this.next = timeline.next;
  }

  finish(): Timeline {
    const pending =
      this.entries.find(
        (entry): entry is Extract<Entry, { kind: "permission" }> =>
          entry.kind === "permission" && entry.outcome === null,
      ) ?? null;
    return {
      entries: this.entries,
      plan: this.plan,
      turns: this.turns,
      inTurn: this.inTurn,
      pending,
      files: this.files,
      status: this.status,
      usage: this.usage,
      next: this.next,
    };
  }

  apply(record: EventRecord): void {
    this.next = record.seq + 1;
    const base = { key: `r${record.seq}`, seq: record.seq, at: record.at };
    const event = record.event;
    switch (event.type) {
      case "status":
        this.status = event.status;
        if (
          event.status.state !== "starting" &&
          event.status.state !== "running"
        ) {
          this.inTurn = false;
        }
        this.entries.push({ ...base, kind: "status", status: event.status });
        return;
      case "terminal":
        return;
      case "profile_changed":
        this.entries.push({
          ...base,
          kind: "profile",
          from: event.from ?? null,
          to: event.to ?? null,
        });
        return;
      case "agent":
        this.agent(base, event.event);
        return;
      case "model_call":
        // The record itself: it is metadata only (route, model, tokens,
        // how it ended, how long).
        this.entries.push({ ...base, kind: "model", call: event });
        this.usage = addCall(this.usage, event);
        return;
    }
  }

  agent(base: Base, event: AgentEvent): void {
    switch (event.event) {
      case "ready":
        this.entries.push({
          ...base,
          kind: "ready",
          harnessSession: event.harness_session,
        });
        return;
      case "turn_started":
        this.turns += 1;
        this.inTurn = true;
        this.entries.push({
          ...base,
          kind: "prompt",
          text: event.content.map(blockText).join(""),
          replayed: false,
          turn: this.turns,
        });
        return;
      case "user_message_chunk":
        this.merge(base, "prompt", blockText(event.content));
        return;
      case "message_chunk":
        this.merge(base, "message", blockText(event.content));
        return;
      case "thought_chunk":
        this.merge(base, "thought", blockText(event.content));
        return;
      case "tool_call":
        this.tool(base, {
          id: event.id,
          title: event.title,
          kind: event.kind,
          status: event.status,
          content: event.content,
          locations: event.locations,
          input: event.raw_input ?? null,
          output: null,
        });
        return;
      case "tool_call_update":
        this.update(base, event);
        return;
      case "plan":
        this.plan = event.entries;
        this.entries.push({ ...base, kind: "plan", entries: event.entries });
        return;
      case "permission_requested": {
        const call = this.findTool(event.tool_call);
        this.entries.push({
          ...base,
          key: `p${event.id}`,
          kind: "permission",
          request: {
            id: event.id,
            tool_call: event.tool_call,
            options: event.options,
          },
          title: call?.tool.title ?? "A tool call",
          outcome: null,
        });
        return;
      }
      case "permission_resolved": {
        const index = this.entries.findIndex(
          (entry) =>
            entry.kind === "permission" && entry.request.id === event.id,
        );
        const entry = this.entries[index];
        if (entry?.kind === "permission") {
          this.entries[index] = { ...entry, outcome: event.outcome };
        }
        return;
      }
      case "turn_ended":
        this.inTurn = false;
        this.entries.push({
          ...base,
          kind: "turn_ended",
          reason: event.reason,
        });
        return;
      case "turn_failed":
        this.inTurn = false;
        this.entries.push({
          ...base,
          kind: "turn_failed",
          message: event.message,
        });
        return;
      case "harness_log":
        this.entries.push({ ...base, kind: "log", line: event.line });
        return;
    }
  }

  /** Append `text` to the last entry if it is a `kind` one, else start
   * one (a user chunk outside a turn is a replayed prompt). */
  merge(
    base: Base,
    kind: "prompt" | "message" | "thought",
    text: string,
  ): void {
    const last = this.entries.at(-1);
    if (last?.kind === kind) {
      this.entries[this.entries.length - 1] = {
        ...last,
        text: last.text + text,
      };
      return;
    }
    if (kind === "prompt") {
      this.entries.push({
        ...base,
        kind,
        text,
        replayed: true,
        turn: this.turns,
      });
    } else {
      this.entries.push({ ...base, kind, text });
    }
  }

  findTool(id: string): Extract<Entry, { kind: "tool" }> | undefined {
    return this.entries.find(
      (entry): entry is Extract<Entry, { kind: "tool" }> =>
        entry.kind === "tool" && entry.tool.id === id,
    );
  }

  tool(base: Base, tool: ToolState): void {
    this.touch(tool.locations);
    const index = this.entries.findIndex(
      (entry) => entry.kind === "tool" && entry.tool.id === tool.id,
    );
    const existing = this.entries[index];
    if (existing?.kind === "tool") {
      this.entries[index] = { ...existing, tool };
      return;
    }
    this.entries.push({ ...base, key: `t${tool.id}`, kind: "tool", tool });
  }

  update(
    base: Base,
    update: Extract<AgentEvent, { event: "tool_call_update" }>,
  ): void {
    const existing = this.findTool(update.id)?.tool;
    const tool: ToolState = {
      id: update.id,
      title: update.title ?? existing?.title ?? "Tool call",
      kind: update.kind ?? existing?.kind ?? "other",
      status: update.status ?? existing?.status ?? "pending",
      content: update.content ?? existing?.content ?? [],
      locations: update.locations ?? existing?.locations ?? [],
      input: update.raw_input ?? existing?.input ?? null,
      output: update.raw_output ?? existing?.output ?? null,
    };
    this.tool(base, tool);
  }

  touch(locations: readonly ToolLocation[]): void {
    for (const location of locations) {
      if (!this.files.includes(location.path)) {
        this.files.push(location.path);
      }
    }
  }
}
