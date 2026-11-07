import { describe, expect, it } from "vitest";

import type { ModelCall, ProfileInfo } from "../wire/proto";
import {
  addCall,
  byProvider,
  callParts,
  duration,
  ended,
  profileOn,
  tokens,
  usageLine,
} from "./models";

const CALL: ModelCall = {
  route: "qwen-coder",
  model: "qwen/qwen3-coder",
  api: "openai_chat_completions",
  usage: {
    input: 72,
    cache_read: 128,
    cache_write: 0,
    output: 50,
    reasoning: 10,
  },
  stop: "tool_use",
  duration_ms: 1234,
};

describe("model calls", () => {
  it("say what `vessel log` says, shorter", () => {
    expect(callParts(CALL)).toEqual([
      "200 in (128 cached)",
      "50 out",
      "tool use",
      "1.2 s",
    ]);
    expect(
      callParts({
        ...CALL,
        usage: {
          input: 0,
          cache_read: 0,
          cache_write: 0,
          output: 0,
          reasoning: 0,
        },
        stop: "error",
        failure: "context_too_long",
        duration_ms: 40,
      }),
    ).toEqual(["0 in", "0 out", "failed: context too long", "40 ms"]);
    expect(ended({ ...CALL, failure: null })).toBe("tool use");
  });

  it("shorten counts and times", () => {
    expect(tokens(950)).toBe("950");
    expect(tokens(1000)).toBe("1k");
    expect(tokens(1234)).toBe("1.2k");
    expect(tokens(48_400)).toBe("48k");
    expect(tokens(1_310_000)).toBe("1.3M");
    expect(duration(999)).toBe("999 ms");
    expect(duration(59_900)).toBe("59.9 s");
    expect(duration(125_000)).toBe("2 m 5 s");
  });

  it("find a profile on a route", () => {
    const profiles: ProfileInfo[] = [
      { name: "work", harness: "claude-code", sets: [] },
      { name: "qwen", harness: "claude-code", sets: [], model: "qwen-coder" },
    ];
    expect(profileOn(profiles, "qwen-coder")?.name).toBe("qwen");
    expect(profileOn(profiles, "other")).toBeUndefined();
  });

  it("sum a session's calls into one line", () => {
    const one = addCall(null, CALL);
    expect(usageLine(one)).toBe("1 model call · 200 in · 50 out");
    const two = addCall(one, { ...CALL, failure: "timeout", stop: "error" });
    expect(usageLine(two)).toBe("2 model calls · 1 failed · 400 in · 100 out");
    expect(two.models).toEqual(["qwen/qwen3-coder"]);
  });

  it("group routes by provider, in the node's order", () => {
    const route = (name: string, provider: string) => ({
      name,
      provider,
      api: "openai_chat_completions" as const,
      model: `${provider}/${name}`,
    });
    const groups = byProvider([
      route("b", "openrouter"),
      route("a", "ollama"),
      route("c", "openrouter"),
    ]);
    expect(
      groups.map(([provider, list]) => [provider, list.map((r) => r.name)]),
    ).toEqual([
      ["openrouter", ["b", "c"]],
      ["ollama", ["a"]],
    ]);
  });
});
