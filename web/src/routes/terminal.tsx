// A terminal session (DESIGN.md §6.8): xterm.js in the design's frame.
//
// The view replays the session's terminal records from the start, as
// `vessel attach` does, then follows live. Keys go to the session as
// input. The size is sent only by a tab that has focus and has typed, so
// a tab that only watches never resizes the harness (Paseo's rule). Keys
// and sizes go out one send at a time, in order (data/terminal-sender.ts).
// A routed session's model calls (protocol 7) are summed into the frame's
// chrome; the records hold metadata only.

import "@xterm/xterm/css/xterm.css";

import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { useEffect, useRef, useState } from "react";

import { useFleet } from "../context";
import { userInput } from "../data/terminal-input";
import { openExternal } from "../data/links";
import { addCall, type Usage, usageLine } from "../data/models";
import { decodeBytes } from "../data/terminal";
import { TerminalSender } from "../data/terminal-sender";
import { Banner } from "../ui/parts";
import { toast } from "../ui/toasts";
import type { StreamState } from "../wire/bridge";
import type { SessionInfo } from "../wire/proto";

/** Code and terminal stack: real monospace faces, then Pretendard, then
 * `monospace` (the operator's font rule). */
const TERMINAL_FONT =
  'Consolas, "SF Mono", Menlo, "JetBrains Mono", "Cascadia Mono", "DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono", Pretendard, monospace';

/** A design token's value, for xterm's theme. */
function token(name: string): string {
  return getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
}

export function TerminalView({
  node,
  label,
  session,
}: {
  node: string;
  label: string;
  session: SessionInfo;
}) {
  const fleet = useFleet();
  const host = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState<{ cols: number; rows: number } | null>(null);
  const [stream, setStream] = useState<StreamState | null>(null);
  // Kept with the session and the next record it expects, so a follow
  // that starts again from the first record counts no call twice.
  const [usage, setUsage] = useState<{
    id: string;
    next: number;
    usage: Usage;
  } | null>(null);
  useEffect(() => {
    const element = host.current;
    if (element === null) {
      return undefined;
    }
    const terminal = new Terminal({
      fontFamily: TERMINAL_FONT,
      fontSize: 13,
      lineHeight: 1.25,
      cursorBlink: true,
      cursorStyle: "block",
      convertEol: false,
      scrollback: 5000,
      // OSC 8 hyperlinks from the session follow the timeline's link
      // policy (data/links.ts): http(s) only, after a confirm showing the
      // URL, in a new isolated tab; the UI tab is never navigated.
      linkHandler: {
        activate: (_event, uri) => {
          openExternal(uri);
        },
        allowNonHttpProtocols: false,
      },
      theme: {
        background: token("--term-bg"),
        foreground: token("--term-fg"),
        cursor: token("--accent"),
        selectionBackground: token("--accent-muted"),
      },
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(element);
    fit.fit();
    setSize({ cols: terminal.cols, rows: terminal.rows });
    // One send in flight, in typing order (data/terminal-sender.ts).
    const sender = new TerminalSender(
      (terminalInput) =>
        fleet.bridge.call(node, "session.send", {
          session: session.id,
          input: { type: "terminal", input: terminalInput },
        }),
      () => {
        toast("Keys dropped: the session is not reading its input", "error");
      },
    );
    let typed = false;
    const sendSize = () => {
      if (!typed || !element.contains(document.activeElement)) {
        return;
      }
      sender.resize(terminal.cols, terminal.rows);
    };
    const input = userInput(terminal, (data) => {
      const first = !typed;
      typed = true;
      if (first) {
        sendSize();
      }
      sender.keys(data);
    });
    const observer = new ResizeObserver(() => {
      fit.fit();
      setSize({ cols: terminal.cols, rows: terminal.rows });
      sendSize();
    });
    observer.observe(element);
    const stop = fleet.bridge.follow(node, session.id, 0, {
      record: (record) => {
        const event = record.event;
        if (event.type === "terminal") {
          terminal.write(decodeBytes(event.data));
        } else if (event.type === "model_call") {
          setUsage((current) => {
            const mine = current?.id === session.id ? current : null;
            if (mine !== null && record.seq < mine.next) {
              return current;
            }
            return {
              id: session.id,
              next: record.seq + 1,
              usage: addCall(mine?.usage ?? null, event),
            };
          });
        }
      },
      state: setStream,
    });
    return () => {
      stop();
      observer.disconnect();
      input.dispose();
      sender.close();
      terminal.dispose();
    };
  }, [fleet, node, session.id]);
  const frozen = stream?.state === "reconnecting" || stream?.state === "failed";
  const calls = usage?.id === session.id ? usage.usage : null;
  return (
    <>
      {stream?.state === "reconnecting" ? (
        <Banner>
          Reconnecting to {label} · {stream.message} · the terminal is frozen
        </Banner>
      ) : stream?.state === "failed" ? (
        <Banner tone="error">The stream stopped: {stream.message}</Banner>
      ) : null}
      <div className="term-wrap">
        <div className="term-frame">
          <div className="term-inner">
            <div className="term-chrome">
              <span>{label}</span>
              <span>{session.id.slice(0, 4)}</span>
              <span>{size === null ? "" : `${size.cols}×${size.rows}`}</span>
              <span>utf-8</span>
              {calls === null ? null : (
                <span className="calls" title={calls.models.join(", ")}>
                  {usageLine(calls)}
                </span>
              )}
              <span className="live">
                {frozen
                  ? "PTY · frozen"
                  : stream?.state === "ended"
                    ? "PTY · ended"
                    : "PTY · live"}
              </span>
            </div>
            <div
              className="term xterm-host"
              ref={host}
              aria-label="Session terminal"
            />
          </div>
        </div>
      </div>
    </>
  );
}
