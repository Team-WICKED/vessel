// xterm's public onData merges user input with replies to terminal queries.
// Its pinned 6.0 core emits onUserInput synchronously immediately before
// onData ONLY for keys, paste and IME. Use that provenance instead of an
// escape-sequence denylist or a replay flag: asynchronous parsing and live
// remote output must never produce input, even while a user is typing.
// This narrow internal adapter is intentional; the real-xterm browser
// regression must pass before upgrading xterm. Missing API fails closed.
import type { IDisposable, Terminal } from "@xterm/xterm";

interface InputCore {
  _core: {
    coreService: {
      onUserInput: (listener: () => void) => IDisposable;
    };
  };
}

export function userInput(
  terminal: Terminal,
  send: (data: string) => void,
): IDisposable {
  const core = (terminal as Terminal & InputCore)._core.coreService;
  let user = false;
  const provenance = core.onUserInput(() => {
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
