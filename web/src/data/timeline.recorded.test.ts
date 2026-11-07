// The reducer against records a real node produced: a structured turn of
// `fake-acp` with its permission answered, recorded through the bridge by
// vessel-web's test `a_structured_turn_with_a_permission_reaches_the_page`
// (rerun it with VESSEL_WRITE_FIXTURES=1 to record them again).

import { describe, expect, it } from "vitest";

import type { EventRecord } from "../wire/proto";
import recorded from "./fixtures/fake-acp-turn.json";
import { EMPTY, reduce } from "./timeline";

// JSON imports are typed loosely (strings, not the wire's literal unions);
// the records are the wire's, as the Rust side wrote them.
const records = recorded as unknown as EventRecord[];

describe("the timeline over recorded records", () => {
  it("folds a whole turn", () => {
    const timeline = reduce(EMPTY, records);
    expect(timeline.entries.map((entry) => entry.kind)).toEqual([
      "status",
      "log",
      "ready",
      "status",
      "prompt",
      "thought",
      "message",
      "tool",
      "plan",
      "permission",
      "turn_ended",
    ]);
    const tool = timeline.entries.find((entry) => entry.kind === "tool");
    expect(tool?.kind === "tool" && tool.tool.status).toBe("completed");
    const permission = timeline.entries.find(
      (entry) => entry.kind === "permission",
    );
    expect(permission?.kind === "permission" && permission.outcome).toEqual({
      outcome: "selected",
      option: "allow-once",
    });
    expect(timeline.pending).toBeNull();
    expect(timeline.inTurn).toBe(false);
    expect(timeline.turns).toBe(1);
    expect(timeline.next).toBe(records.length);
  });

  it("gives the same timeline however the records are batched", () => {
    const whole = reduce(EMPTY, records);
    let piecewise = EMPTY;
    for (const record of records) {
      piecewise = reduce(piecewise, [record]);
    }
    expect(piecewise.entries).toEqual(whole.entries);
    expect(piecewise.plan).toEqual(whole.plan);
  });
});
