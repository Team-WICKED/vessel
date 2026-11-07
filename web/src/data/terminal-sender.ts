// A terminal view's keys and sizes on their way to the session, in order.
//
// [Decision Log]
// - Purpose: what a person types reaches the harness in the order it was
//   typed, a paste and the Enter after it included.
// - Constraint: the bridge runs a tab's calls concurrently, and the node
//   runs a connection's calls concurrently too, so two `session.send`
//   calls in flight at once can reach the PTY in either order. (The e2e
//   run in CI caught a pasted line arriving after its Enter.)
// - Choice: the CLI's attach sender (crates/vessel-cli/src/attach/
//   terminal/sender.rs) in the browser: one send in flight per view; keys
//   typed meanwhile are joined into one send, and a size queued just
//   before another is replaced, keys and sizes keeping their order.
// - Bound: at most `MAX_PENDING` characters of keys wait. Past that,
//   keys are dropped and `dropped` is told once per overflow: a browser
//   cannot stop xterm producing input the way the CLI stops reading its
//   terminal, and an unbounded queue behind a harness that stopped reading
//   would grow without end.
// - Failures: a send that fails is not repeated (it may have been
//   applied); the stream's own state says why, and later items still go,
//   in order.

import type { TerminalInput } from "../wire/proto";
import { encodeText } from "./terminal";

/** Most characters of keys waiting while a send is in flight. */
export const MAX_PENDING = 1024 * 1024;

type Item =
  | { kind: "keys"; text: string }
  | { kind: "resize"; cols: number; rows: number };

export class TerminalSender {
  readonly #send: (input: TerminalInput) => Promise<unknown>;
  readonly #dropped: () => void;
  readonly #items: Item[] = [];
  #chars = 0;
  #busy = false;
  #closed = false;
  #overflowing = false;

  constructor(
    send: (input: TerminalInput) => Promise<unknown>,
    dropped: () => void = () => undefined,
  ) {
    this.#send = send;
    this.#dropped = dropped;
  }

  /** Queue keys, joined to keys queued just before. */
  keys(text: string): void {
    if (this.#closed || text === "") {
      return;
    }
    if (this.#chars + text.length > MAX_PENDING) {
      if (!this.#overflowing) {
        this.#overflowing = true;
        this.#dropped();
      }
      return;
    }
    this.#overflowing = false;
    this.#chars += text.length;
    const last = this.#items.at(-1);
    if (last?.kind === "keys") {
      last.text += text;
    } else {
      this.#items.push({ kind: "keys", text });
    }
    this.#pump();
  }

  /** Queue a size; a size queued just before is replaced. */
  resize(cols: number, rows: number): void {
    if (this.#closed) {
      return;
    }
    const last = this.#items.at(-1);
    if (last?.kind === "resize") {
      last.cols = cols;
      last.rows = rows;
    } else {
      this.#items.push({ kind: "resize", cols, rows });
    }
    this.#pump();
  }

  /** Drop what has not been sent; send nothing more. */
  close(): void {
    this.#closed = true;
    this.#items.length = 0;
    this.#chars = 0;
  }

  #pump(): void {
    if (this.#busy) {
      return;
    }
    const item = this.#items.shift();
    if (item === undefined || this.#closed) {
      return;
    }
    if (item.kind === "keys") {
      this.#chars -= item.text.length;
    }
    this.#busy = true;
    const input: TerminalInput =
      item.kind === "keys"
        ? { kind: "data", data: encodeText(item.text) }
        : { kind: "resize", size: { cols: item.cols, rows: item.rows } };
    this.#send(input)
      .catch(() => undefined)
      .finally(() => {
        this.#busy = false;
        this.#pump();
      });
  }
}
