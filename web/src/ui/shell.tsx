// The chrome every page sits in (DESIGN.md §2.2): topbar, sidebar, main.

import {
  Link,
  Outlet,
  useNavigate,
  useRouterState,
} from "@tanstack/react-router";
import {
  Fragment,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactElement,
} from "react";

import { openNewSession, useChrome, useFleet, useFleetState } from "../context";
import type { FleetState } from "../data/fleet";
import { isLive, nodeState, shortId } from "../data/format";
import { nodeLabel, selectedNode } from "../data/nodes";
import { NewSessionDialog } from "../routes/new-session";
import { Banner, Icon, StatusDot } from "./parts";
import { Toasts } from "./toasts";

const MARK = (
  <span className="mark" aria-hidden="true">
    <svg
      width="12"
      height="12"
      viewBox="0 0 12 12"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M2 8.5 6 2.5 10 8.5" />
      <path d="M4 8.5h4" />
    </svg>
  </span>
);

export function Shell(): ReactElement {
  const newSession = useChrome((chrome) => chrome.newSession);
  useShortcuts();
  return (
    <>
      <a className="skip" href="#main">
        Skip to main
      </a>
      <div className="app">
        <Topbar />
        <Sidebar />
        <main className="main" id="main" role="main">
          <Disconnected />
          <Outlet />
        </main>
      </div>
      {newSession === null ? null : <NewSessionDialog node={newSession.node} />}
      <Toasts />
    </>
  );
}

/** After repeated failures the tab stops redialling (bridge/client.ts):
 * say so, and offer to try again with the same key. */
function Disconnected() {
  const fleet = useFleet();
  const link = useFleetState((state) => state.link);
  if (link.kind !== "disconnected") {
    return null;
  }
  return (
    <Banner
      tone="error"
      action={
        <button
          className="btn sm"
          type="button"
          onClick={() => {
            fleet.bridge.retry();
          }}
        >
          Retry
        </button>
      }
    >
      Disconnected — vessel ui may have restarted. If it did, open the address
      it printed.
    </Banner>
  );
}

function Topbar() {
  const crumbs = useChrome((chrome) => chrome.crumbs);
  const link = useFleetState((state) => state.link);
  const selected = useFleetState((state) => selectedNode(state, undefined));
  const chip =
    link.kind === "open"
      ? { tone: "running", text: "vessel ui · connected" }
      : link.kind === "refused"
        ? { tone: "danger", text: "vessel ui · refused" }
        : link.kind === "disconnected"
          ? { tone: "danger", text: "vessel ui · disconnected" }
          : { tone: "warn", text: "vessel ui · reconnecting" };
  return (
    <header className="topbar" role="banner">
      <Link className="brand" to="/fleet">
        {MARK}
        <span>Vessel</span>
      </Link>
      <nav className="crumb" aria-label="Breadcrumb">
        {crumbs.map((crumb, index) => {
          const last = index === crumbs.length - 1;
          return (
            <Fragment key={`${crumb.label}-${index}`}>
              {index > 0 ? <span className="sep">/</span> : null}
              {last || crumb.to === undefined ? (
                <span className={crumb.mono === true ? "here mono" : "here"}>
                  {crumb.label}
                </span>
              ) : (
                <Link to={crumb.to}>{crumb.label}</Link>
              )}
            </Fragment>
          );
        })}
      </nav>
      <Search />
      <div className="topbar-actions">
        <button
          className="btn primary sm"
          type="button"
          onClick={() => {
            openNewSession(selected);
          }}
        >
          <Icon name="plus" size="sm" /> New
        </button>
        <Link
          className={link.kind === "open" ? "chip" : "chip reconnecting"}
          to="/settings"
          title="This client"
        >
          <StatusDot tone={chip.tone} label={chip.text} />
          {chip.text}
        </Link>
      </div>
    </header>
  );
}

interface Hit {
  label: string;
  detail: string;
  to: string;
}

/** Nodes and sessions matching `query`. */
function hits(state: FleetState, query: string): Hit[] {
  const text = query.trim().toLowerCase();
  if (text === "") {
    return [];
  }
  const found: Hit[] = [];
  for (const node of state.book ?? []) {
    const label = nodeLabel(node, state.statuses[node.key]);
    if (label.toLowerCase().includes(text)) {
      found.push({ label, detail: "node", to: `/fleet/${node.key}` });
    }
  }
  for (const [node, sessions] of Object.entries(state.sessions)) {
    for (const session of sessions) {
      const cwd = session.spec.cwd ?? "";
      if (session.id.startsWith(text) || cwd.toLowerCase().includes(text)) {
        found.push({
          label: shortId(session.id),
          detail: `${session.status.state} · ${cwd}`,
          to: `/sessions/${node}/${session.id}`,
        });
      }
    }
  }
  return found.slice(0, 8);
}

function Search() {
  const [query, setQuery] = useState("");
  const input = useRef<HTMLInputElement>(null);
  const navigate = useNavigate();
  const found = useFleetState((state) => state);
  const results = useMemo(() => hits(found, query), [found, query]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const typing =
        event.target instanceof HTMLInputElement ||
        event.target instanceof HTMLTextAreaElement;
      if (
        (event.key === "k" && (event.metaKey || event.ctrlKey)) ||
        (event.key === "/" && !typing)
      ) {
        event.preventDefault();
        input.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
    };
  }, []);
  return (
    <div className="topbar-search-wrap">
      <label className="topbar-search">
        <Icon name="search" />
        <input
          ref={input}
          type="search"
          placeholder="Search nodes, sessions"
          aria-label="Search"
          value={query}
          onChange={(event) => {
            setQuery(event.target.value);
          }}
          onKeyDown={(event) => {
            const first = results[0];
            if (event.key === "Enter" && first !== undefined) {
              void navigate({ to: first.to });
              setQuery("");
              input.current?.blur();
            }
            if (event.key === "Escape") {
              setQuery("");
              input.current?.blur();
            }
          }}
        />
        <kbd className="kbd">⌘K</kbd>
      </label>
      {results.length === 0 ? null : (
        <ul className="palette" role="listbox" aria-label="Results">
          {results.map((hit) => (
            <li key={hit.to} role="option" aria-selected="false">
              <Link
                to={hit.to}
                onClick={() => {
                  setQuery("");
                }}
              >
                <span className="mono">{hit.label}</span>
                <span className="muted">{hit.detail}</span>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Sidebar() {
  const book = useFleetState((state) => state.book);
  const statuses = useFleetState((state) => state.statuses);
  const sessions = useFleetState((state) => state.sessions);
  const path = useRouterState({ select: (state) => state.location.pathname });
  const live = Object.values(sessions).reduce(
    (total, list) => total + list.filter(isLive).length,
    0,
  );
  return (
    <nav className="sidebar" role="navigation" aria-label="Fleet">
      <div className="nav-block">
        <div className="nav-kicker">Place</div>
        <Link
          className={path === "/fleet" ? "nav-item active" : "nav-item"}
          to="/fleet"
        >
          <Icon name="fleet" />
          <span className="label">Fleet</span>
        </Link>
        {(book ?? []).map((node) => {
          const status = statuses[node.key];
          const state = nodeState(status);
          const count = sessions[node.key]?.filter(isLive).length;
          const connected = status?.state === "connected";
          const active =
            path.startsWith(`/fleet/${node.key}`) ||
            path.startsWith(`/sessions/${node.key}/`);
          return (
            <Link
              key={node.key}
              className={`nav-item node${connected ? "" : " offline"}${active ? " active" : ""}`}
              to="/fleet/$node"
              params={{ node: node.key }}
            >
              <StatusDot tone={state.tone} label={state.word} />
              <span className="label">{nodeLabel(node, status)}</span>
              {connected && count !== undefined ? (
                <span className="count">{count}</span>
              ) : (
                <span className="dash">—</span>
              )}
            </Link>
          );
        })}
      </div>
      <div className="nav-block">
        <div className="nav-kicker">Work</div>
        <Link
          className={path === "/sessions" ? "nav-item active" : "nav-item"}
          to="/sessions"
        >
          <Icon name="sessions" />
          <span className="label">Sessions</span>
          <span className="count">{live}</span>
        </Link>
        <Link
          className={path === "/sync" ? "nav-item active" : "nav-item"}
          to="/sync"
        >
          <Icon name="sync" />
          <span className="label">Sync</span>
        </Link>
        <Link
          className={path === "/devices" ? "nav-item active" : "nav-item"}
          to="/devices"
        >
          <Icon name="devices" />
          <span className="label">Devices</span>
        </Link>
      </div>
      <div className="nav-foot">
        <Link
          className={path === "/settings" ? "nav-item active" : "nav-item"}
          to="/settings"
        >
          <Icon name="settings" />
          <span className="label">Settings</span>
        </Link>
      </div>
    </nav>
  );
}

/** `g f` / `g s` / `g y` / `g d` (DESIGN.md §7.1). */
function useShortcuts(): void {
  const navigate = useNavigate();
  useEffect(() => {
    let leader = false;
    const onKey = (event: KeyboardEvent) => {
      const target = event.target;
      if (
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement ||
        (target instanceof HTMLElement && target.closest(".xterm") !== null)
      ) {
        return;
      }
      if (leader) {
        leader = false;
        const to = { f: "/fleet", s: "/sessions", y: "/sync", d: "/devices" }[
          event.key
        ];
        if (to !== undefined) {
          void navigate({ to });
        }
        return;
      }
      leader = event.key === "g";
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
    };
  }, [navigate]);
}
