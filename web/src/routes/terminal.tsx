// A terminal session (DESIGN.md §6.8): xterm.js in the design's frame.
//
// The view replays the session's terminal records from the start, as
// `vessel attach` does, then follows live. Keys go to the session as
// input. The size is sent only by a tab that has focus and has typed, so
// a tab that only watches never resizes the harness (Paseo's rule).

import "@xterm/xterm/css/xterm.css";

import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import { useEffect, useRef, useState } from "react";

import { useFleet } from "../context";
import { decodeBytes, encodeText } from "../data/terminal";
import { Banner } from "../ui/parts";
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
    let typed = false;
    const sendSize = () => {
      if (!typed || !element.contains(document.activeElement)) {
        return;
      }
      void fleet.bridge.call(node, "session.send", {
        session: session.id,
        input: {
          type: "terminal",
          input: {
            kind: "resize",
            size: { cols: terminal.cols, rows: terminal.rows },
          },
        },
      });
    };
    const input = terminal.onData((data) => {
      const first = !typed;
      typed = true;
      if (first) {
        sendSize();
      }
      void fleet.bridge
        .call(node, "session.send", {
          session: session.id,
          input: {
            type: "terminal",
            input: { kind: "data", data: encodeText(data) },
          },
        })
        .catch(() => {
          // The stream's state says why (the node is down, the session
          // ended); the key is not sent twice.
        });
    });
    const observer = new ResizeObserver(() => {
      fit.fit();
      setSize({ cols: terminal.cols, rows: terminal.rows });
      sendSize();
    });
    observer.observe(element);
    const stop = fleet.bridge.follow(node, session.id, 0, {
      record: (record) => {
        if (record.event.type === "terminal") {
          terminal.write(decodeBytes(record.event.data));
        }
      },
      state: setStream,
    });
    return () => {
      stop();
      observer.disconnect();
      input.dispose();
      terminal.dispose();
    };
  }, [fleet, node, session.id]);
  const frozen = stream?.state === "reconnecting" || stream?.state === "failed";
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
