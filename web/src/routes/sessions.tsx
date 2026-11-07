// Sessions (DESIGN.md §2.4): live and recent sessions on one node or all,
// as a tree: a session an agent started nests under the session whose
// agent started it, on whichever node it runs (ADR 0013).

import { Link, useNavigate } from "@tanstack/react-router";
import { useState, type ReactElement } from "react";

import { openNewSession, useCrumbs, useFleet, useFleetState } from "../context";
import type { FleetState } from "../data/fleet";
import {
  ago,
  harnessName,
  isLive,
  shortId,
  statusTone,
  statusWord,
} from "../data/format";
import { useWhere } from "../data/where";
import { labelOf, nodeLabel } from "../data/nodes";
import { liveOnly, type Row, tree } from "../data/tree";
import { Badge, Empty, Icon } from "../ui/parts";
import { toast } from "../ui/toasts";

/** Who started a session, in words. */
function creator(state: FleetState, row: Row): string {
  const by = row.session.created_by;
  switch (by.kind) {
    case "owner":
      return "you · owner token";
    case "device":
      return `device ${by.id.slice(0, 6)}`;
    case "agent":
      return row.parent === null
        ? `agent of ${shortId(by.session)}`
        : `agent of ${shortId(by.session)} on ${labelOf(state, row.node)}`;
  }
}

type Filter = "live" | "recent";

export function SessionsPage(): ReactElement {
  const where = useWhere();
  const scope = where.node ?? null;
  const fleet = useFleet();
  const state = useFleetState((value) => value);
  const navigate = useNavigate();
  const [filter, setFilter] = useState<Filter>("live");
  const book = state.book ?? [];
  const nodes = scope === null ? book.map((node) => node.key) : [scope];
  const scopeLabel = scope === null ? null : labelOf(state, scope);
  useCrumbs(
    scopeLabel === null
      ? [{ label: "Sessions" }]
      : [{ label: "Sessions", to: "/sessions" }, { label: scopeLabel }],
    scope ?? "",
  );
  const rows =
    filter === "recent" ? tree(state, nodes) : liveOnly(tree(state, nodes));
  const kill = async (row: Row) => {
    try {
      await fleet.bridge.call(row.node, "session.kill", {
        session: row.session.id,
      });
      toast(`Killed ${shortId(row.session.id)}`);
      void fleet.refreshSessions(row.node);
    } catch (error) {
      toast(error instanceof Error ? error.message : String(error), "error");
    }
  };
  return (
    <div className="page">
      <div className="page-head">
        <div>
          <h1>Sessions</h1>
          <p>
            {scopeLabel === null ? "All nodes." : `On ${scopeLabel}.`}{" "}
            Agent-created sessions nest under the session that started them.
          </p>
        </div>
        <button
          className="btn primary"
          type="button"
          onClick={() => {
            openNewSession(scope);
          }}
        >
          <Icon name="plus" size="sm" /> New session
        </button>
      </div>
      <div className="filters" aria-label="Filters">
        <div className="seg">
          <Link
            className={scope === null ? "active" : undefined}
            to="/sessions"
          >
            All nodes
          </Link>
          {book.map((node) => (
            <Link
              key={node.key}
              className={scope === node.key ? "active" : undefined}
              to="/sessions"
              search={{ node: node.key }}
            >
              {nodeLabel(node, state.statuses[node.key])}
            </Link>
          ))}
        </div>
        <div className="seg">
          {(["live", "recent"] as const).map((value) => (
            <button
              key={value}
              type="button"
              className={filter === value ? "active" : undefined}
              onClick={() => {
                setFilter(value);
              }}
            >
              {value === "live" ? "Live" : "Recent"}
            </button>
          ))}
        </div>
        <span className="spacer" />
        <span className="hint">Enter opens the focused row</span>
      </div>
      <div className="page-body flush">
        {rows.length === 0 ? (
          <Empty
            title={
              scopeLabel === null
                ? filter === "live"
                  ? "No live sessions"
                  : "No sessions"
                : `No ${filter === "live" ? "live " : ""}sessions on ${scopeLabel}`
            }
            action={
              <button
                className="btn primary"
                type="button"
                onClick={() => {
                  openNewSession(scope);
                }}
              >
                New session
              </button>
            }
          >
            {filter === "live"
              ? "Recent shows the ones that ended."
              : "Start one to see it here."}
          </Empty>
        ) : (
          <div className="table-wrap flat">
            <table className="data" aria-label="Sessions">
              <thead>
                <tr>
                  <th scope="col">Session</th>
                  <th scope="col">Node</th>
                  <th scope="col">Harness</th>
                  <th scope="col">Model / cwd</th>
                  <th scope="col">Status</th>
                  <th scope="col">Started</th>
                  <th scope="col">
                    <span className="visually-hidden">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => {
                  const open = () => {
                    void navigate({
                      to: "/sessions/$node/$id",
                      params: { node: row.node, id: row.session.id },
                    });
                  };
                  const spec = row.session.spec;
                  const model =
                    spec.mode === "structured"
                      ? (spec.model ?? "harness default")
                      : null;
                  return (
                    <tr
                      key={row.session.id}
                      tabIndex={0}
                      aria-level={row.depth + 1}
                      onClick={open}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") {
                          open();
                        }
                      }}
                    >
                      <td>
                        <div
                          className={
                            row.depth === 0
                              ? "tree-pad"
                              : `tree-pad l${Math.min(row.depth, 2)}`
                          }
                        >
                          {row.depth === 0 ? null : (
                            <span className="tree-rail" aria-hidden="true" />
                          )}
                          <Icon
                            name={
                              spec.mode === "terminal" ? "terminal" : "message"
                            }
                            size="sm"
                          />
                          <div className="stack">
                            <span className="name mono">
                              {shortId(row.session.id)}
                            </span>
                            <span className="muted">{creator(state, row)}</span>
                          </div>
                        </div>
                      </td>
                      <td className="mono">{labelOf(state, row.node)}</td>
                      <td>
                        <Badge
                          tone={spec.mode === "structured" ? "accent" : ""}
                        >
                          {spec.mode}
                        </Badge>
                        <span className="muted">
                          {" "}
                          {row.session.profile ?? harnessName(spec.harness)}
                        </span>
                      </td>
                      <td>
                        <div className="stack">
                          {model === null ? null : (
                            <span className="mono">{model}</span>
                          )}
                          <span className="path">{spec.cwd ?? "~"}</span>
                        </div>
                      </td>
                      <td>
                        <Badge tone={statusTone(row.session.status)}>
                          {statusWord(row.session.status)}
                        </Badge>
                      </td>
                      <td className="mono muted">
                        {ago(row.session.created_at)}
                      </td>
                      <td>
                        <div className="row-actions">
                          <Link
                            className="btn ghost sm icon-btn"
                            to="/sessions/$node/$id"
                            params={{ node: row.node, id: row.session.id }}
                            aria-label={`Open ${shortId(row.session.id)}`}
                            onClick={(event) => {
                              event.stopPropagation();
                            }}
                          >
                            <Icon name="chevron" size="sm" />
                          </Link>
                          {isLive(row.session) ? (
                            <button
                              className="btn ghost sm icon-btn"
                              type="button"
                              aria-label={`Kill ${shortId(row.session.id)}`}
                              onClick={(event) => {
                                event.stopPropagation();
                                void kill(row);
                              }}
                            >
                              <Icon name="kill" size="sm" />
                            </button>
                          ) : null}
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
