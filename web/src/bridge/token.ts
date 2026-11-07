// The launch's token (ADR 0017). `vessel ui` opens the page with
// `#token=<hex>`; the fragment never reaches a server. It is kept in
// `localStorage` (scoped to this origin, port included, so another
// launch's page never sees it) and removed from the address bar at once,
// so it is not left in history, bookmarks or a screenshot.

const KEY = "vessel.token";
const FRAGMENT = /(?:^#|&)token=([0-9a-f]{64})(?:&|$)/;

/** The token from the URL's fragment (kept, then removed from the
 * address), else the one kept from earlier in this launch. */
export function takeToken(): string | null {
  const match = FRAGMENT.exec(window.location.hash);
  const token = match?.[1];
  if (token !== undefined) {
    window.localStorage.setItem(KEY, token);
    window.history.replaceState(
      window.history.state,
      "",
      window.location.pathname + window.location.search,
    );
    return token;
  }
  return window.localStorage.getItem(KEY);
}

/** Forget a token the server refused (a new launch has a new one). */
export function forgetToken(): void {
  window.localStorage.removeItem(KEY);
}
