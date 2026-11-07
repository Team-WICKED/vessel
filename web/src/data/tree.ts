// The Sessions list as a tree (DESIGN.md §2.4; ADR 0013): a session an
// agent started nests under the session whose agent started it, on
// whichever node either runs.

import type { SessionInfo } from "../wire/proto";
import type { FleetState } from "./fleet";
import { isLive } from "./format";

/** A session with the node it runs on and its place in the tree. */
export interface Row {
  node: string;
  session: SessionInfo;
  depth: number;
  parent: SessionInfo | null;
}

/** Every session of `nodes` as tree rows: roots (started by a person)
 * newest first, each followed by its agent-started children, depth first.
 * A child whose parent is not listed is a root. */
export function tree(state: FleetState, nodes: readonly string[]): Row[] {
  const all: { node: string; session: SessionInfo }[] = nodes.flatMap((node) =>
    (state.sessions[node] ?? []).map((session) => ({ node, session })),
  );
  const byId = new Map(all.map((entry) => [entry.session.id, entry]));
  const children = new Map<string, { node: string; session: SessionInfo }[]>();
  const roots: { node: string; session: SessionInfo }[] = [];
  for (const entry of all) {
    const creator = entry.session.created_by;
    if (creator.kind === "agent" && byId.has(creator.session)) {
      const list = children.get(creator.session) ?? [];
      list.push(entry);
      children.set(creator.session, list);
    } else {
      roots.push(entry);
    }
  }
  const newest = (
    one: { session: SessionInfo },
    two: { session: SessionInfo },
  ) => two.session.created_at.localeCompare(one.session.created_at);
  const rows: Row[] = [];
  const visit = (
    entry: { node: string; session: SessionInfo },
    depth: number,
    parent: SessionInfo | null,
  ) => {
    rows.push({ ...entry, depth, parent });
    for (const child of (children.get(entry.session.id) ?? []).sort(newest)) {
      visit(child, depth + 1, entry.session);
    }
  };
  for (const root of roots.sort(newest)) {
    visit(root, 0, null);
  }
  return rows;
}

/** The rows that are live, with the ancestors of live rows (a parent
 * that ended still shows where its live children came from). */
export function liveOnly(rows: readonly Row[]): Row[] {
  const kept: Row[] = [];
  // Walking backwards, `below` holds whether a live row sits under each
  // depth seen so far.
  let liveBelow: boolean[] = [];
  for (const row of [...rows].reverse()) {
    const child = liveBelow[row.depth + 1] === true;
    const keep = isLive(row.session) || child;
    liveBelow = liveBelow.slice(0, row.depth + 1);
    liveBelow[row.depth] = keep || liveBelow[row.depth] === true;
    if (keep) {
      kept.push(row);
    }
  }
  return kept.reverse();
}
