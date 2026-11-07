// Where the page is: path segments and the `node` query parameter.
//
// Read from the router's location rather than its typed params hooks: with
// code-based routes whose components are typed through the same router,
// TanStack Router's loose (`strict: false`) params and search come out as
// `any`, which the lint rules (no `any`) refuse. The pathname and the
// query string are plain strings either way.

import { useRouterState } from "@tanstack/react-router";

export interface Where {
  /** The path's segments, decoded (`/sessions/box/3f2a` → sessions, box, 3f2a). */
  segments: readonly string[];
  /** `?node=`, when given. */
  node: string | undefined;
}

/** A query value as the router writes it: plain text, or JSON text. */
function queryValue(raw: string | null): string | undefined {
  if (raw === null) {
    return undefined;
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    return typeof parsed === "string" ? parsed : raw;
  } catch {
    return raw;
  }
}

export function useWhere(): Where {
  const pathname = useRouterState({
    select: (state) => state.location.pathname,
  });
  const query = useRouterState({ select: (state) => state.location.searchStr });
  const segments = pathname
    .split("/")
    .filter((segment) => segment !== "")
    .map((segment) => decodeURIComponent(segment));
  return { segments, node: queryValue(new URLSearchParams(query).get("node")) };
}
