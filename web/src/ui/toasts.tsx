// Toasts (DESIGN.md §6.10): bottom right, success leaves after 4 s, an
// error stays until dismissed.

import { Store, useStore } from "../data/store";

interface Toast {
  id: number;
  tone: "ok" | "error";
  text: string;
}

const toasts = new Store<readonly Toast[]>([]);
let next = 1;

export function toast(text: string, tone: "ok" | "error" = "ok"): void {
  const id = next;
  next += 1;
  toasts.update((list) => [...list, { id, tone, text }]);
  if (tone === "ok") {
    setTimeout(() => {
      dismiss(id);
    }, 4000);
  }
}

function dismiss(id: number): void {
  toasts.update((list) => list.filter((item) => item.id !== id));
}

export function Toasts() {
  const list = useStore(toasts, (value) => value);
  return (
    <div className="toast-stack">
      {list.map((item) => (
        <div
          key={item.id}
          className={item.tone === "error" ? "toast error" : "toast"}
          role={item.tone === "error" ? "alert" : "status"}
        >
          <span>{item.text}</span>
          <button
            className="btn ghost sm icon-btn"
            type="button"
            aria-label="Dismiss"
            onClick={() => {
              dismiss(item.id);
            }}
          >
            ×
          </button>
        </div>
      ))}
    </div>
  );
}
