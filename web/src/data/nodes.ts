// Which node is meant, and what to call it (DESIGN.md §2.5, §9).

import type { BookNode, NodeKey, NodeStatus } from "../wire/bridge";
import type { FleetState } from "./fleet";

/** A node's name for display: the book's, else (this machine's node) the
 * name it gives itself once connected. */
export function nodeLabel(
  node: BookNode,
  status: NodeStatus | undefined,
): string {
  if (node.key !== "_here") {
    return node.label;
  }
  return status?.state === "connected" ? status.node.name : node.label;
}

/** The label of `key`, from the book. */
export function labelOf(state: FleetState, key: NodeKey): string {
  const node = state.book?.find((candidate) => candidate.key === key);
  return node === undefined ? key : nodeLabel(node, state.statuses[key]);
}

/** The node a scoped page means: the URL's, else the last chosen, else the
 * book's default, else this machine's. */
export function selectedNode(
  state: FleetState,
  fromUrl: string | undefined,
): NodeKey | null {
  if (fromUrl !== undefined) {
    return fromUrl;
  }
  if (state.selected !== null) {
    return state.selected;
  }
  const book = state.book ?? [];
  return (book.find((node) => node.default) ?? book[0])?.key ?? null;
}
