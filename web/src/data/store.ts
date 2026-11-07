// A small external store for React (`useSyncExternalStore`): the app's
// state lives outside components, which subscribe to the slice they show
// (ADR 0017, decision 7: no query library; the bridge pushes most state).

import { useSyncExternalStore } from "react";

export class Store<T> {
  #value: T;
  readonly #listeners = new Set<() => void>();

  constructor(value: T) {
    this.#value = value;
  }

  readonly get = (): T => this.#value;

  set(value: T): void {
    if (Object.is(value, this.#value)) {
      return;
    }
    this.#value = value;
    for (const listener of this.#listeners) {
      listener();
    }
  }

  update(change: (value: T) => T): void {
    this.set(change(this.#value));
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };
}

/** `select(store's value)`, re-rendering when it changes. `select` must
 * return a value already in the store (or a primitive), not a new object
 * on every call. */
export function useStore<T, S>(store: Store<T>, select: (value: T) => S): S {
  return useSyncExternalStore(store.subscribe, () => select(store.get()));
}
