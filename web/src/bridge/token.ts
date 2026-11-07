// History may retain a URL fragment even after replaceState. It therefore
// carries only a one-use code, never the independent session key. Keep the
// latter in this tab's sessionStorage for reloads, not origin localStorage.
import type { LaunchId } from "../wire/bridge";

export type Credential =
  | { kind: "bootstrap"; key: string }
  | { kind: "session"; key: string; launch: LaunchId };
const KEY = "vessel.session";
const HEX = /^[0-9a-f]{64}$/;

export function takeToken(): Credential | null {
  // Remove legacy credentials on upgrade, even when opening a fresh URL.
  window.localStorage.removeItem("vessel.token");
  const fragment = window.location.hash;
  window.history.replaceState(
    window.history.state,
    "",
    window.location.pathname + window.location.search,
  );
  const code = new URLSearchParams(fragment.slice(1)).get("bootstrap");
  if (code !== null && HEX.test(code)) {
    window.sessionStorage.removeItem(KEY);
    return { kind: "bootstrap", key: code };
  }
  const saved = window.sessionStorage.getItem(KEY);
  if (saved === null) return null;
  const [launch, key, extra] = saved.split(":");
  if (
    launch === undefined ||
    key === undefined ||
    extra !== undefined ||
    !HEX.test(launch) ||
    !HEX.test(key)
  )
    return null;
  return { kind: "session", launch, key };
}

export function rememberSession(credential: Credential): void {
  if (credential.kind === "session") {
    window.sessionStorage.setItem(
      KEY,
      `${credential.launch}:${credential.key}`,
    );
  }
}
export function forgetToken(): void {
  window.localStorage.removeItem("vessel.token");
  window.sessionStorage.removeItem(KEY);
}
