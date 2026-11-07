// How the UI writes protocol values: one vocabulary, the wire's
// (DESIGN.md §1.1). Statuses keep their protocol names.

import type { NodeStatus } from "../wire/bridge";
import type { HarnessKind, SessionInfo, SessionStatus } from "../wire/proto";

/** The first four characters of an id: what the chrome shows. */
export function shortId(id: string): string {
  return id.slice(0, 4);
}

/** The word for a session's status. */
export function statusWord(status: SessionStatus): string {
  switch (status.state) {
    case "exited":
      return status.code === null ? "exited" : `exited ${status.code}`;
    case "starting":
    case "running":
    case "failed":
    case "interrupted":
      return status.state;
  }
}

/** The badge class for a session's status (DESIGN.md §6.4). */
export function statusTone(status: SessionStatus): string {
  switch (status.state) {
    case "starting":
      return "warn";
    case "running":
      return "running";
    case "failed":
      return "danger";
    case "interrupted":
      return "interrupted";
    case "exited":
      return "";
  }
}

/** Whether a session counts as live (the sidebar's count). */
export function isLive(session: SessionInfo): boolean {
  return (
    session.status.state === "starting" || session.status.state === "running"
  );
}

/** A harness as the operator types it. */
export function harnessName(kind: HarnessKind): string {
  switch (kind) {
    case "claude-code":
      return "claude";
    case "codex":
      return "codex";
    case "gemini":
      return "gemini";
    case "open-code":
      return "opencode";
  }
}

/** "3m", "2h", "5d" since `iso`, at `now`. */
export function ago(iso: string, now: number = Date.now()): string {
  const seconds = Math.max(0, Math.round((now - Date.parse(iso)) / 1000));
  if (seconds < 60) {
    return `${seconds}s`;
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 48) {
    return `${hours}h`;
  }
  return `${Math.round(hours / 24)}d`;
}

/** A local time of day, or a date when not today. */
export function when(iso: string): string {
  const date = new Date(iso);
  const today = new Date();
  const time = date.toLocaleTimeString("en-GB", {
    hour: "2-digit",
    minute: "2-digit",
  });
  if (date.toDateString() === today.toDateString()) {
    return `today ${time}`;
  }
  return `${date.toLocaleDateString("en-CA")} ${time}`;
}

/** The word and tone for a node's link, as the sidebar and badges show it. */
export function nodeState(status: NodeStatus | undefined): {
  word: string;
  tone: string;
} {
  switch (status?.state) {
    case undefined:
    case "idle":
    case "connecting":
      return { word: "connecting", tone: "warn" };
    case "connected":
      return status.node.state === "draining"
        ? { word: "draining", tone: "warn" }
        : { word: "serving", tone: "running" };
    case "reconnecting":
      return { word: "reconnecting", tone: "warn" };
    case "failed":
      return { word: "offline", tone: "" };
  }
}
