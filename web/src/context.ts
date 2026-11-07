// The fleet controller, shared through React context, and the page-level
// UI state the chrome needs (the breadcrumb, the new-session dialog).

import { createContext, useContext, useEffect } from "react";

import type { Fleet, FleetState } from "./data/fleet";
import { Store, useStore } from "./data/store";

export const FleetContext = createContext<Fleet | null>(null);

/** The fleet controller. */
export function useFleet(): Fleet {
  const fleet = useContext(FleetContext);
  if (fleet === null) {
    throw new Error("useFleet outside the app");
  }
  return fleet;
}

/** A slice of the fleet's state (see `useStore` for what `select` may
 * return). */
export function useFleetState<S>(select: (state: FleetState) => S): S {
  return useStore(useFleet().store, select);
}

/** One step of the topbar's breadcrumb. */
export interface Crumb {
  label: string;
  to?: string;
  mono?: boolean;
}

export interface Chrome {
  crumbs: readonly Crumb[];
  /** The new-session dialog, open with this node preselected. */
  newSession: { node: string | null } | null;
}

export const chrome = new Store<Chrome>({ crumbs: [], newSession: null });

/** Set the breadcrumb while a page is shown. `key` changes when it does. */
export function useCrumbs(crumbs: readonly Crumb[], key: string): void {
  useEffect(() => {
    chrome.update((state) => ({ ...state, crumbs }));
    // `crumbs` is rebuilt every render; `key` says when it changed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
}

export function useChrome<S>(select: (state: Chrome) => S): S {
  return useStore(chrome, select);
}

export function openNewSession(node: string | null): void {
  chrome.update((state) => ({ ...state, newSession: { node } }));
}

export function closeNewSession(): void {
  chrome.update((state) => ({ ...state, newSession: null }));
}
