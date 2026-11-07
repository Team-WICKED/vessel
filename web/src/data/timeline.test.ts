import { describe, expect, it } from "vitest";

import type { AgentEvent, EventRecord, SessionStatus } from "../wire/proto";
import { EMPTY, reduce } from "./timeline";

let seq = 0;
const at = "2026-11-07T12:00:00Z";

function agent(event: AgentEvent): EventRecord {
  const record: EventRecord = { seq, at, event: { type: "agent", event } };
  seq += 1;
  return record;
}

function status(value: SessionStatus): EventRecord {
  const record: EventRecord = {
    seq,
    at,
    event: { type: "status", status: value },
  };
  seq += 1;
  return record;
}

/** One `fake-acp` turn, as its doc describes it, up to the permission. */
function turn(): EventRecord[] {
  seq = 0;
  return [
    status({ state: "running" }),
    agent({ event: "ready", harness_session: "fake-session-1" }),
    agent({
      event: "turn_started",
      content: [{ type: "text", text: "fix it" }],
    }),
    agent({
      event: "thought_chunk",
      content: { type: "text", text: "thinking" },
    }),
    agent({
      event: "message_chunk",
      content: { type: "text", text: "echo: fix it " },
    }),
    agent({
      event: "message_chunk",
      content: { type: "text", text: "(model fake-small)" },
    }),
    agent({
      event: "tool_call",
      id: "call-1",
      title: "Read status.rs",
      kind: "read",
      status: "pending",
      content: [],
      locations: [{ path: "/src/app/crates/fleet/src/status.rs", line: 12 }],
      raw_input: { path: "status.rs" },
    }),
    agent({
      event: "plan",
      entries: [
        { content: "Read the code", priority: "high", status: "in_progress" },
      ],
    }),
    agent({ event: "tool_call_update", id: "call-1", status: "in_progress" }),
    agent({
      event: "permission_requested",
      id: "perm-1",
      tool_call: "call-1",
      options: [
        { id: "allow-once", name: "Allow once", kind: "allow_once" },
        { id: "reject-once", name: "Reject once", kind: "reject_once" },
      ],
    }),
  ];
}

describe("the timeline", () => {
  it("folds a turn into entries, merging chunks and updating tools in place", () => {
    const timeline = reduce(EMPTY, turn());
    expect(timeline.entries.map((entry) => entry.kind)).toEqual([
      "status",
      "ready",
      "prompt",
      "thought",
      "message",
      "tool",
      "plan",
      "permission",
    ]);
    const message = timeline.entries[4];
    expect(message?.kind === "message" && message.text).toBe(
      "echo: fix it (model fake-small)",
    );
    const tool = timeline.entries[5];
    expect(tool?.kind === "tool" && tool.tool.status).toBe("in_progress");
    expect(tool?.key).toBe("tcall-1");
    expect(timeline.turns).toBe(1);
    expect(timeline.inTurn).toBe(true);
    expect(timeline.pending?.title).toBe("Read status.rs");
    expect(timeline.files).toEqual(["/src/app/crates/fleet/src/status.rs"]);
    expect(timeline.plan?.[0]?.content).toBe("Read the code");
    expect(timeline.next).toBe(10);
  });

  it("resolves a permission and ends the turn", () => {
    const first = reduce(EMPTY, turn());
    const second = reduce(first, [
      agent({
        event: "permission_resolved",
        id: "perm-1",
        outcome: { outcome: "selected", option: "allow-once" },
      }),
      agent({
        event: "tool_call_update",
        id: "call-1",
        status: "completed",
        raw_output: { ok: true },
      }),
      agent({ event: "turn_ended", reason: "end_turn" }),
    ]);
    expect(second.pending).toBeNull();
    expect(second.inTurn).toBe(false);
    const permission = second.entries.find(
      (entry) => entry.kind === "permission",
    );
    expect(permission?.kind === "permission" && permission.outcome).toEqual({
      outcome: "selected",
      option: "allow-once",
    });
    // Entries that did not change are the same objects.
    expect(second.entries[2]).toBe(first.entries[2]);
    expect(second.entries.at(-1)?.kind).toBe("turn_ended");
  });

  it("skips records it has seen, so a resumed stream can overlap", () => {
    const records = turn();
    const once = reduce(EMPTY, records);
    expect(reduce(once, records)).toBe(once);
    const halves = reduce(reduce(EMPTY, records.slice(0, 5)), records);
    expect(halves.entries).toEqual(once.entries);
  });

  it("marks user chunks outside a turn as a replayed prompt", () => {
    seq = 0;
    const timeline = reduce(EMPTY, [
      agent({
        event: "user_message_chunk",
        content: { type: "text", text: "earlier " },
      }),
      agent({
        event: "user_message_chunk",
        content: { type: "text", text: "question" },
      }),
      agent({
        event: "message_chunk",
        content: { type: "text", text: "replayed" },
      }),
    ]);
    const prompt = timeline.entries[0];
    expect(prompt?.kind === "prompt" && prompt.replayed).toBe(true);
    expect(prompt?.kind === "prompt" && prompt.text).toBe("earlier question");
  });
});
