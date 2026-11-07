// Model routing as the page shows it (milestone 5, ADR 0015): a node's
// routes, the route a profile names, and a session's model calls.
//
// [Decision Log]
// - Purpose: a routed session runs its harness on another provider's
//   model through the node's proxy. The operator picks the route when
//   starting a session and sees, per call, which model answered, the
//   tokens, how it ended and how long it took.
// - Constraint: a route reaches a session only through a profile naming it
//   (`model = "<route>"` in `profiles.toml`); `session.create` has no
//   route field, and a routed harness is pinned to the route's models, so
//   asking it for another model fails. A `ModelCall` record holds
//   metadata only (never prompt, completion or key), and the page shows
//   no more than that.
// - Choice: the New session dialog's Model field lists the harness default
//   and the node's routes (`model.list`); choosing a route chooses a
//   profile of that harness on it, and choosing a routed profile shows its
//   route. Words and number formats here mirror `vessel log` and `vessel
//   models` (crates/vessel-cli/src/render.rs), so the CLI and the page
//   describe a call the same way.
// - Cost: a route no profile of the harness names cannot be picked; the
//   dialog says so instead of hiding it, since `vessel models` lists it.

import type {
  ModelApi,
  ModelCall,
  ModelFailure,
  ModelRoute,
  ModelStop,
  ProfileInfo,
  SessionInfo,
  TokenUsage,
} from "../wire/proto";

/** An API's name, as `vessel models` prints it. */
export function apiName(api: ModelApi): string {
  switch (api) {
    case "anthropic_messages":
      return "anthropic-messages";
    case "openai_responses":
      return "openai-responses";
    case "openai_chat_completions":
      return "openai-chat-completions";
  }
}

/** How a call ended, as `vessel log` says it. */
export function stopWord(stop: ModelStop): string {
  switch (stop) {
    case "end_turn":
      return "end turn";
    case "tool_use":
      return "tool use";
    case "max_tokens":
      return "max tokens";
    case "stop_sequence":
      return "stop sequence";
    case "refusal":
      return "refused";
    case "pause":
      return "paused";
    case "error":
      return "error";
  }
}

/** Why a call failed, in words. */
export function failureWord(failure: ModelFailure): string {
  return failure.replaceAll("_", " ");
}

/** A token count, short: `950`, `1.2k`, `48k`, `1.3M`. */
export function tokens(count: number): string {
  if (count < 1000) {
    return String(count);
  }
  const [unit, size] = count < 1_000_000 ? ["k", 1000] : ["M", 1_000_000];
  const scaled = count / size;
  const text = scaled < 10 ? scaled.toFixed(1) : Math.round(scaled).toFixed(0);
  return `${text.replace(/\.0$/, "")}${unit}`;
}

/** A duration in milliseconds, short: `40 ms`, `1.2 s`, `2 m 5 s`. */
export function duration(ms: number): string {
  if (ms < 1000) {
    return `${String(ms)} ms`;
  }
  if (ms < 60_000) {
    return `${(ms / 1000).toFixed(1)} s`;
  }
  const seconds = Math.round(ms / 1000);
  return `${String(Math.floor(seconds / 60))} m ${String(seconds % 60)} s`;
}

/** Every input token, cached or not (the proxy counts `input` without
 * the cache, on every API). */
export function totalInput(usage: TokenUsage): number {
  return usage.input + usage.cache_read + usage.cache_write;
}

/** How a call ended, failure first. */
export function ended(call: ModelCall): string {
  return call.failure === undefined || call.failure === null
    ? stopWord(call.stop)
    : `failed: ${failureWord(call.failure)}`;
}

/** The parts of one call's line: tokens, how it ended, how long. */
export function callParts(call: ModelCall): string[] {
  const usage = call.usage;
  const cached = usage.cache_read + usage.cache_write;
  return [
    cached > 0
      ? `${tokens(totalInput(usage))} in (${tokens(cached)} cached)`
      : `${tokens(usage.input)} in`,
    `${tokens(usage.output)} out`,
    ended(call),
    duration(call.duration_ms),
  ];
}

/** A route in one line: its provider and model. */
export function routeLine(route: ModelRoute): string {
  return `${route.provider} · ${route.model}`;
}

/** The model a session runs on: its route's (it is pinned to it), the
 * one it asked for, or its harness's default. */
export function sessionModel(session: SessionInfo): string {
  if (session.model !== undefined && session.model !== null) {
    return session.model.model;
  }
  return session.spec.mode === "structured"
    ? (session.spec.model ?? "harness default")
    : "harness default";
}

/** `routes` grouped by provider, in the order the node lists them (the
 * design's picker groups by provider, DESIGN.md §6.2). */
export function byProvider(
  routes: readonly ModelRoute[],
): [string, ModelRoute[]][] {
  const groups = new Map<string, ModelRoute[]>();
  for (const route of routes) {
    const group = groups.get(route.provider);
    if (group === undefined) {
      groups.set(route.provider, [route]);
    } else {
      group.push(route);
    }
  }
  return [...groups];
}

/** The first of `profiles` on route `route`. */
export function profileOn(
  profiles: readonly ProfileInfo[],
  route: string,
): ProfileInfo | undefined {
  return profiles.find((profile) => profile.model === route);
}

/** A session's model calls, summed. */
export interface Usage {
  calls: number;
  failed: number;
  /** Input tokens outside the prompt cache. */
  input: number;
  cacheRead: number;
  cacheWrite: number;
  /** Output tokens, reasoning included. */
  output: number;
  reasoning: number;
  ms: number;
  /** The upstream models that answered, in first-call order. */
  models: readonly string[];
}

/** `usage` with `call` added. */
export function addCall(usage: Usage | null, call: ModelCall): Usage {
  const sum = usage ?? {
    calls: 0,
    failed: 0,
    input: 0,
    cacheRead: 0,
    cacheWrite: 0,
    output: 0,
    reasoning: 0,
    ms: 0,
    models: [],
  };
  const failed = call.failure !== undefined && call.failure !== null;
  return {
    calls: sum.calls + 1,
    failed: sum.failed + (failed ? 1 : 0),
    input: sum.input + call.usage.input,
    cacheRead: sum.cacheRead + call.usage.cache_read,
    cacheWrite: sum.cacheWrite + call.usage.cache_write,
    output: sum.output + call.usage.output,
    reasoning: sum.reasoning + call.usage.reasoning,
    ms: sum.ms + call.duration_ms,
    models: sum.models.includes(call.model)
      ? sum.models
      : [...sum.models, call.model],
  };
}

/** A session's calls in one line: `3 model calls · 1.2k in · 300 out`. */
export function usageLine(usage: Usage): string {
  const calls = `${String(usage.calls)} model call${usage.calls === 1 ? "" : "s"}`;
  const failed = usage.failed === 0 ? [] : [`${String(usage.failed)} failed`];
  return [
    calls,
    ...failed,
    `${tokens(usage.input + usage.cacheRead + usage.cacheWrite)} in`,
    `${tokens(usage.output)} out`,
  ].join(" · ");
}
