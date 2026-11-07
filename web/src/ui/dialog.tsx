// A modal dialog (DESIGN.md §6.9): dimmer, Escape and the dimmer cancel,
// focus moves in on open and back on close.

import { type ReactNode, useEffect, useId, useRef } from "react";

import { Icon } from "./parts";

export function Dialog({
  title,
  onClose,
  children,
  footer,
  wide = false,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
}) {
  const titleId = useId();
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const opener = document.activeElement;
    const first = box.current?.querySelector<HTMLElement>(
      "input, select, textarea, button",
    );
    first?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      if (opener instanceof HTMLElement) {
        opener.focus();
      }
    };
  }, [onClose]);
  return (
    <div
      className="overlay"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <div
        ref={box}
        className={wide ? "dialog" : "dialog narrow"}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
      >
        <div className="dialog-h">
          <h1 id={titleId}>{title}</h1>
          <button
            className="btn ghost icon-btn"
            type="button"
            aria-label="Close"
            onClick={onClose}
          >
            <Icon name="close" />
          </button>
        </div>
        <div className="dialog-b">{children}</div>
        {footer === undefined ? null : <div className="dialog-f">{footer}</div>}
      </div>
    </div>
  );
}
