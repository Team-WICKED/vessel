// xterm's public onData merges user input with replies to terminal queries.
// Its pinned 6.0 core emits onUserInput synchronously immediately before
// onData ONLY for keys, paste and IME. Use that provenance instead of an
// escape-sequence denylist or a replay flag: asynchronous parsing and live
// remote output must never produce input, even while a user is typing.
// This narrow internal adapter is intentional; the real-xterm browser
// regression must pass before upgrading xterm. Missing API fails closed.
import type { IDisposable, Terminal } from "@xterm/xterm";

type OnUserInput = (listener: () => void) => IDisposable;

/** xterm's internal `onUserInput`, if this build still has it. */
function onUserInput(terminal: Terminal): OnUserInput | null {
  const core: unknown = Reflect.get(terminal, "_core");
  if (typeof core !== "object" || core === null) return null;
  const service: unknown = Reflect.get(core, "coreService");
  if (typeof service !== "object" || service === null) return null;
  const listen: unknown = Reflect.get(service, "onUserInput");
  if (typeof listen !== "function") return null;
  return (listener) => listen.call(service, listener) as IDisposable;
}

/** Send what the user types on `terminal`. Without xterm's provenance
 * signal nothing is sent at all (the terminal stays view-only), so remote
 * output can never be echoed back as input. */
export function userInput(
  terminal: Terminal,
  send: (data: string) => void,
): IDisposable {
  const listen = onUserInput(terminal);
  if (listen === null) {
    console.error(
      "vessel ui: this xterm has no input provenance; the terminal is view-only",
    );
    return {
      dispose() {
        // Nothing was subscribed.
      },
    };
  }
  let user = false;
  const provenance = listen(() => {
    user = true;
  });
  const data = terminal.onData((text) => {
    const accepted = user;
    user = false;
    if (accepted) {
      send(text);
    }
  });
  return {
    dispose() {
      provenance.dispose();
      data.dispose();
    },
  };
}
