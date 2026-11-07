// Small parts shared by the screens (DESIGN.md §6): icons, badges, status
// dots, empty states, banners, skeleton rows. Class names are the design's.

import type { ReactNode } from "react";

export type IconName =
  | "fleet"
  | "sessions"
  | "sync"
  | "devices"
  | "settings"
  | "plus"
  | "search"
  | "close"
  | "chevron"
  | "chevron-down"
  | "terminal"
  | "message"
  | "copy"
  | "more"
  | "home"
  | "warn"
  | "check"
  | "kill"
  | "move"
  | "link"
  | "plan"
  | "read"
  | "edit"
  | "exec"
  | "search-kind"
  | "qr"
  | "dot";

/** A stroke icon in the current text colour (a CSS mask). */
export function Icon({ name, size }: { name: IconName; size?: "sm" | "lg" }) {
  return (
    <span
      className={`ico ico-${name}${size === undefined ? "" : ` ${size}`}`}
      aria-hidden="true"
    />
  );
}

/** A status badge: the word is always there (status is never colour only). */
export function Badge({
  tone = "",
  children,
}: {
  tone?: string;
  children: ReactNode;
}) {
  return (
    <span className={tone === "" ? "badge" : `badge ${tone}`}>{children}</span>
  );
}

/** A status dot with its word for screen readers. */
export function StatusDot({ tone, label }: { tone: string; label: string }) {
  const kind = tone === "running" ? "" : tone === "" ? "offline" : tone;
  return (
    <span
      className={kind === "" ? "status-dot" : `status-dot ${kind}`}
      aria-label={label}
      role="img"
    />
  );
}

/** An empty list: specific copy, one action (DESIGN.md §6.11). */
export function Empty({
  title,
  children,
  action,
}: {
  title: string;
  children?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="empty">
      <Icon name="dot" />
      <h2>{title}</h2>
      {children === undefined ? null : <p>{children}</p>}
      {action}
    </div>
  );
}

/** An error or a warning across the top of a pane. */
export function Banner({
  tone = "warn",
  children,
  action,
}: {
  tone?: "warn" | "error";
  children: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div
      className={tone === "error" ? "banner error" : "banner"}
      role={tone === "error" ? "alert" : "status"}
      aria-live="assertive"
    >
      <Icon name="warn" size="sm" />
      <span>{children}</span>
      {action}
    </div>
  );
}

/** Three loading rows. */
export function Skeleton() {
  return (
    <div className="stack" aria-label="Loading" role="status">
      <div className="skel" />
      <div className="skel" />
      <div className="skel" />
    </div>
  );
}

/** A copy-to-clipboard button. */
export function CopyButton({
  text,
  label,
  onCopied,
}: {
  text: string;
  label: string;
  onCopied?: () => void;
}) {
  return (
    <button
      className="btn ghost sm icon-btn"
      type="button"
      aria-label={label}
      title={label}
      onClick={() => {
        void navigator.clipboard.writeText(text).then(onCopied);
      }}
    >
      <Icon name="copy" size="sm" />
    </button>
  );
}
