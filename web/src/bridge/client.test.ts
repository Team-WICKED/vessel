// Regression for a stale tab reconnecting to a local port squatter.
import { createHmac } from "node:crypto";
import { afterEach, expect, test, vi } from "vitest";
import { Bridge } from "./client";
import type { ServerFrame } from "../wire/bridge";

vi.mock("./token", () => ({ rememberSession: vi.fn() }));
const key = "a".repeat(64);
const launch = "b".repeat(64);
class Socket extends EventTarget {
  static OPEN = 1;
  static all: Socket[] = [];
  readyState = 1;
  sent: string[] = [];
  constructor() {
    super();
    Socket.all.push(this);
  }
  send(data: string) {
    this.sent.push(data);
  }
  close() {
    this.readyState = 3;
  }
  message(frame: unknown) {
    this.dispatchEvent(
      new MessageEvent("message", { data: JSON.stringify(frame) }),
    );
  }
  cut() {
    this.dispatchEvent(Object.assign(new Event("close"), { code: 1006 }));
  }
}
const bridges: Bridge[] = [];
function start() {
  vi.stubGlobal("WebSocket", Socket);
  const bridge = new Bridge(
    { kind: "session", key, launch },
    "ws://127.0.0.1:1/api/ws",
  );
  bridges.push(bridge);
  bridge.start();
  const socket = Socket.all.at(-1);
  if (socket === undefined) throw new Error("No socket");
  socket.dispatchEvent(new Event("open"));
  return { bridge, socket };
}
afterEach(() => {
  for (const bridge of bridges) bridge.stop();
  bridges.length = 0;
  Socket.all = [];
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

async function answer(socket: Socket): Promise<ServerFrame> {
  const nonce = "c".repeat(64);
  socket.message({ type: "challenge", launch, nonce });
  await vi.waitFor(() => {
    expect(socket.sent.length).toBe(1);
  });
  const hello = JSON.parse(socket.sent[0] ?? "{}") as {
    nonce: string;
    proof: string;
  };
  expect(socket.sent.join("").includes(key)).toBe(false);
  const version = "0.1.0";
  const proof = createHmac("sha256", key)
    .update(
      `vessel-ui-v1\nserver\n${launch}\n${nonce}\n${hello.nonce}\n${version}\n`,
    )
    .digest("hex");
  return { type: "welcome", launch, version, proof, session: null };
}

test("a version-only fake welcome never opens the bridge or receives a call", async () => {
  const { bridge, socket } = start();
  socket.message({ type: "welcome", version: "0.1.0" });
  await vi.waitFor(() => {
    expect(bridge.link.kind).toBe("refused");
  });
  await expect(
    bridge.op({ op: "inspect", link: "synthetic-private-link" }),
  ).rejects.toThrow();
  expect(socket.sent).toEqual([]);
});

test("a changed launch stops before sending even a proof", async () => {
  const { bridge, socket } = start();
  socket.message({
    type: "challenge",
    launch: "d".repeat(64),
    nonce: "e".repeat(64),
  });
  await vi.waitFor(() => {
    expect(bridge.link.kind).toBe("refused");
  });
  expect(socket.sent).toEqual([]);
});

test("the same launch reconnects with mutual proof, but an old welcome cannot be replayed", async () => {
  const { bridge, socket } = start();
  const welcome = await answer(socket);
  socket.message(welcome);
  await vi.waitFor(() => {
    expect(bridge.link.kind).toBe("open");
  });
  socket.cut();
  await vi.waitFor(() => {
    expect(Socket.all.length).toBe(2);
  });
  const next = Socket.all.at(-1);
  if (next === undefined) throw new Error("No reconnect");
  await answer(next);
  next.message(welcome);
  await vi.waitFor(() => {
    expect(bridge.link.kind).toBe("refused");
  });
  expect(next.sent.length).toBe(1);
});

test("a bogus proof and pre-welcome node data are refused", async () => {
  const { bridge, socket } = start();
  const welcome = await answer(socket);
  socket.message({ ...welcome, proof: "0".repeat(64) });
  socket.message({ type: "node", node: "injected", status: { state: "idle" } });
  await vi.waitFor(() => {
    expect(bridge.link.kind).toBe("refused");
  });
  expect(bridge.statuses().size).toBe(0);
});

test("events from a stopped socket cannot authenticate its replacement", async () => {
  const { bridge, socket } = start();
  const welcome = await answer(socket);
  bridge.stop();
  bridge.start();
  socket.message(welcome);
  const next = Socket.all.at(-1);
  if (next === undefined) throw new Error("No replacement");
  next.message(await answer(next));
  await vi.waitFor(() => {
    expect(bridge.link.kind).toBe("open");
  });
  expect(next.sent.length).toBe(1);
});

test("repeated upgrade failures stop redialling but keep the key", async () => {
  vi.useFakeTimers();
  const { bridge } = start();
  for (let attempt = 0; attempt < 5; attempt += 1) {
    Socket.all.at(-1)?.cut();
    await vi.advanceTimersByTimeAsync(500 * 2 ** attempt);
  }
  // Stopped: no more dials on its own, and not a refusal.
  expect(bridge.link.kind).toBe("disconnected");
  expect(Socket.all.length).toBe(5);
  await vi.advanceTimersByTimeAsync(60_000);
  expect(Socket.all.length).toBe(5);
  // Retry dials again with the same session key, which still proves.
  bridge.retry();
  const next = Socket.all.at(-1);
  if (next === undefined || Socket.all.length !== 6)
    throw new Error("No retry");
  next.dispatchEvent(new Event("open"));
  vi.useRealTimers();
  next.message(await answer(next));
  await vi.waitFor(() => {
    expect(bridge.link.kind).toBe("open");
  });
});

test("a handshake that never completes is retried, not refused", async () => {
  vi.useFakeTimers();
  const { bridge } = start();
  await vi.advanceTimersByTimeAsync(10_000);
  expect(bridge.link.kind).toBe("closed");
  await vi.advanceTimersByTimeAsync(500);
  expect(Socket.all.length).toBe(2);
});

test("a refusal is final: Retry does not redial", () => {
  const { bridge, socket } = start();
  socket.dispatchEvent(Object.assign(new Event("close"), { code: 4401 }));
  expect(bridge.link.kind).toBe("refused");
  bridge.retry();
  expect(Socket.all.length).toBe(1);
});
