import { beforeAll, describe, expect, it, vi } from "vitest";

import type { TerminalInput } from "../wire/proto";
import { MAX_PENDING, TerminalSender } from "./terminal-sender";

beforeAll(() => {
  // The page's base64 helpers use the browser's `btoa`.
  vi.stubGlobal("window", {
    btoa: (binary: string) => Buffer.from(binary, "latin1").toString("base64"),
  });
});

/** A send that answers only when the test says so. */
function sends() {
  const sent: string[] = [];
  const answers: ((ok: boolean) => void)[] = [];
  const send = (input: TerminalInput) => {
    sent.push(
      input.kind === "data"
        ? Buffer.from(input.data, "base64").toString("utf8")
        : `<${String(input.size.cols)}x${String(input.size.rows)}>`,
    );
    return new Promise<void>((resolve, reject) => {
      answers.push((ok) => {
        if (ok) {
          resolve();
        } else {
          reject(new Error("dropped"));
        }
      });
    });
  };
  /** Answer the oldest send, then let its continuation run. */
  const answer = async (ok = true) => {
    answers.shift()?.(ok);
    await new Promise((resolve) => setTimeout(resolve, 0));
  };
  return { sent, send, answer, inFlight: () => answers.length };
}

describe("terminal input", () => {
  it("is sent one call at a time, in the order typed", async () => {
    const { sent, send, answer, inFlight } = sends();
    const sender = new TerminalSender(send);
    sender.resize(120, 40);
    sender.keys("pasted line");
    sender.keys("\r");
    expect(sent).toEqual(["<120x40>"]);
    expect(inFlight()).toBe(1);
    await answer();
    // Keys typed during the resize went out together, Enter last.
    expect(sent).toEqual(["<120x40>", "pasted line\r"]);
    await answer();
    expect(inFlight()).toBe(0);
  });

  it("keeps keys and sizes in order, the latest size only", async () => {
    const { sent, send, answer } = sends();
    const sender = new TerminalSender(send);
    sender.keys("a");
    sender.keys("b");
    sender.resize(80, 24);
    sender.resize(100, 30);
    sender.keys("c");
    await answer();
    await answer();
    await answer();
    expect(sent).toEqual(["a", "b", "<100x30>", "c"]);
  });

  it("goes on after a failed send, without repeating it", async () => {
    const { sent, send, answer } = sends();
    const sender = new TerminalSender(send);
    sender.keys("one");
    sender.keys("two");
    await answer(false);
    await answer();
    expect(sent).toEqual(["one", "two"]);
  });

  it("drops keys past its bound and says so once", async () => {
    const { sent, send, answer } = sends();
    let dropped = 0;
    const sender = new TerminalSender(send, () => {
      dropped += 1;
    });
    sender.keys("x");
    sender.keys("y".repeat(MAX_PENDING));
    sender.keys("z");
    sender.keys("w");
    expect(dropped).toBe(1);
    await answer();
    expect(sent[1]?.length).toBe(MAX_PENDING);
    // Room again: keys are taken.
    sender.keys("v");
    await answer();
    expect(sent.slice(2)).toEqual(["v"]);
  });

  it("sends nothing after close", async () => {
    const { sent, send, answer } = sends();
    const sender = new TerminalSender(send);
    sender.keys("a");
    sender.keys("b");
    sender.close();
    await answer();
    sender.keys("c");
    expect(sent).toEqual(["a"]);
  });
});
