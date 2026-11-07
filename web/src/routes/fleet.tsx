// Fleet (DESIGN.md §3.1): every node the book knows, with what each
// daemon reports; a node's facts in the inspector.

import type { ReactElement } from "react";
import { Link, useNavigate } from "@tanstack/react-router";

import { openNewSession, useCrumbs, useFleet, useFleetState } from "../context";
import type { FleetState } from "../data/fleet";
import { harnessName, isLive, nodeState, shortId, when } from "../data/format";
import { nodeLabel } from "../data/nodes";
import { useWhere } from "../data/where";
import type { BookNode, NodeStatus } from "../wire/bridge";
import { Badge, Banner, Empty, Icon, Skeleton, StatusDot } from "../ui/parts";

export function FleetPage(): ReactElement {
  const where = useWhere();
  const chosen = where.segments[1] ?? null;
  const fleet = useFleet();
  const book = useFleetState((state) => state.book);
  const bookError = useFleetState((state) => state.bookError);
  const statuses = useFleetState((state) => state.statuses);
  const sessions = useFleetState((state) => state.sessions);
  const navigate = useNavigate();
  const nodes = book ?? [];
  const selected = nodes.find((node) => node.key === chosen) ?? null;
  useCrumbs(
    selected === null
      ? [{ label: "Fleet" }]
      : [
          { label: "Fleet", to: "/fleet" },
          { label: nodeLabel(selected, statuses[selected.key]) },
        ],
    chosen ?? "",
  );
  const online = nodes.filter(
    (node) => statuses[node.key]?.state === "connected",
  ).length;
  const live = Object.values(sessions).flatMap((list) => list.filter(isLive));
  const byAgents = live.filter(
    (session) => session.created_by.kind === "agent",
  ).length;
  const harnessCount = new Set(
    Object.values(useFleetState((state) => state.harnesses)).flatMap((list) =>
      list.map((harness) => harness.kind),
    ),
  ).size;

  return (
    <div className="page">
      <div className="page-head">
        <div>
          <h1>Fleet</h1>
          <p>
            {nodes.length === 1 ? "One node" : `${nodes.length} nodes`} in this
            client&apos;s book. Live counts are starting or running sessions.
          </p>
        </div>
        <button
          className="btn"
          type="button"
          onClick={() => {
            openNewSession(chosen);
          }}
        >
          <Icon name="plus" size="sm" /> New session
        </button>
      </div>
      <div className="page-body">
        {bookError === null ? null : (
          <Banner
            tone="error"
            action={
              <button
                className="btn sm"
                type="button"
                onClick={() => void fleet.loadBook()}
              >
                Retry
              </button>
            }
          >
            The node book could not be read: {bookError}
          </Banner>
        )}
        <div className="strip" aria-label="Fleet summary">
          <div>
            <div className="k">Nodes</div>
            <div className="v mono">{nodes.length}</div>
            <div className="s">
              {online} online · {nodes.length - online} not connected
            </div>
          </div>
          <div>
            <div className="k">Live sessions</div>
            <div className="v mono">{live.length}</div>
            <div className="s">
              {byAgents === 0
                ? "all started by a person"
                : `${byAgents} of them agent-created`}
            </div>
          </div>
          <div>
            <div className="k">Harnesses</div>
            <div className="v mono">{harnessCount}</div>
            <div className="s">kinds installed across the fleet</div>
          </div>
          <div>
            <div className="k">Reach</div>
            <div className="v mono">{online}</div>
            <div className="s">through vessel ui on this machine</div>
          </div>
        </div>
        {book === null ? (
          <Skeleton />
        ) : nodes.length === 0 ? (
          <Empty title="No nodes in the book">
            Add one with <code>vessel nodes add NAME ADDRESS</code>, or pair one
            in Settings.
          </Empty>
        ) : (
          <div className={selected === null ? "" : "two-col"}>
            <div className="table-wrap">
              <table className="data" aria-label="Nodes">
                <thead>
                  <tr>
                    <th scope="col">Node</th>
                    <th scope="col">State</th>
                    <th scope="col">Reach</th>
                    <th scope="col">Harnesses</th>
                    <th scope="col" className="right">
                      Live
                    </th>
                    <th scope="col">Version</th>
                  </tr>
                </thead>
                <tbody>
                  {nodes.map((node) => (
                    <NodeRow
                      key={node.key}
                      node={node}
                      status={statuses[node.key]}
                      selected={node.key === chosen}
                      open={() => {
                        fleet.select(node.key);
                        void navigate({
                          to: "/fleet/$node",
                          params: { node: node.key },
                        });
                      }}
                    />
                  ))}
                </tbody>
              </table>
            </div>
            {selected === null ? null : <NodePanel node={selected} />}
          </div>
        )}
      </div>
    </div>
  );
}

function reach(node: BookNode): string {
  switch (node.transport) {
    case "ws":
      return "direct";
    case "ssh":
      return "ssh";
    case "paired":
      return "relay";
  }
}

function NodeRow({
  node,
  status,
  selected,
  open,
}: {
  node: BookNode;
  status: NodeStatus | undefined;
  selected: boolean;
  open: () => void;
}) {
  const sessions = useFleetState((state) => state.sessions[node.key]);
  const harnesses = useFleetState((state) => state.harnesses[node.key]);
  const state = nodeState(status);
  const info = status?.state === "connected" ? status.node : null;
  const detail =
    info === null
      ? `${node.address} · ${state.word}`
      : `${info.os}/${info.arch} · ${node.transport === "ws" ? "websocket" : node.transport}`;
  return (
    <tr
      className={selected ? "selected" : undefined}
      onClick={open}
      tabIndex={0}
      onKeyDown={(event) => {
        if (event.key === "Enter") {
          open();
        }
      }}
    >
      <td>
        <div className="inline">
          <StatusDot tone={state.tone} label={state.word} />
          <div className="stack">
            <span className="name">{nodeLabel(node, status)}</span>
            <span className="muted">{detail}</span>
          </div>
        </div>
      </td>
      <td>
        <Badge tone={state.tone}>{state.word}</Badge>
      </td>
      <td>
        <Badge tone="mono">{reach(node)}</Badge>
      </td>
      <td className="mono muted">
        {harnesses === undefined
          ? "—"
          : harnesses.map((harness) => harnessName(harness.kind)).join(", ")}
      </td>
      <td className="mono right">
        {info === null || sessions === undefined
          ? "—"
          : sessions.filter(isLive).length}
      </td>
      <td className="mono muted">{info === null ? "—" : info.version}</td>
    </tr>
  );
}

function NodePanel({ node }: { node: BookNode }) {
  const fleet = useFleet();
  const status = useFleetState((state: FleetState) => state.statuses[node.key]);
  const harnesses = useFleetState((state) => state.harnesses[node.key]);
  const profiles = useFleetState((state) => state.profiles[node.key]);
  const sessions = useFleetState((state) => state.sessions[node.key]);
  const info = status?.state === "connected" ? status.node : null;
  const live = (sessions ?? []).filter(isLive);
  return (
    <aside
      className="panel"
      role="complementary"
      aria-label={nodeLabel(node, status)}
    >
      <h2>{nodeLabel(node, status)}</h2>
      <div className="pad">
        <dl>
          <dt>id</dt>
          <dd>{info?.id ?? node.id ?? "not seen yet"}</dd>
          <dt>address</dt>
          <dd>{node.address}</dd>
          {info === null ? (
            <>
              <dt>state</dt>
              <dd>{nodeState(status).word}</dd>
            </>
          ) : (
            <>
              <dt>host</dt>
              <dd>{info.hostname}</dd>
              <dt>os</dt>
              <dd>
                {info.os} / {info.arch}
              </dd>
              <dt>version</dt>
              <dd>
                {info.version} · protocol{" "}
                {status?.state === "connected" ? status.protocol : ""}
              </dd>
              <dt>started</dt>
              <dd>{when(info.started_at)}</dd>
              <dt>state</dt>
              <dd>{info.state}</dd>
            </>
          )}
        </dl>
        {status?.state === "failed" || status?.state === "reconnecting" ? (
          <div className="panel-section">
            <p className="hint">{status.message}</p>
            <button
              className="btn sm"
              type="button"
              onClick={() => {
                void fleet.bridge.op({ op: "retry", node: node.key });
              }}
            >
              Retry now
            </button>
          </div>
        ) : null}
        {harnesses === undefined ? null : (
          <section className="panel-section">
            <h3>Harnesses</h3>
            <div className="stack">
              {harnesses.map((harness) => {
                const names = (profiles ?? [])
                  .filter((profile) => profile.harness === harness.kind)
                  .map((profile) => profile.name);
                return (
                  <div className="inline" key={harness.kind}>
                    <Badge
                      tone={
                        harness.structured.state === "available"
                          ? "running"
                          : ""
                      }
                    >
                      {harnessName(harness.kind)}
                    </Badge>
                    <span className="muted">
                      {harness.version ?? "version unknown"}
                      {names.length === 0 ? "" : ` · ${names.join(", ")}`}
                    </span>
                  </div>
                );
              })}
            </div>
          </section>
        )}
        {sessions === undefined ? null : (
          <section className="panel-section">
            <h3>Live</h3>
            <p className="hint">
              {live.length} live ·{" "}
              <Link to="/sessions" search={{ node: node.key }}>
                open sessions
              </Link>
            </p>
            <div className="stack">
              {live.slice(0, 6).map((session) => (
                <Link
                  key={session.id}
                  className="path"
                  to="/sessions/$node/$id"
                  params={{ node: node.key, id: session.id }}
                >
                  {shortId(session.id)} · {harnessName(session.spec.harness)} ·{" "}
                  {session.spec.mode === "terminal"
                    ? "terminal"
                    : (session.spec.cwd ?? "~")}
                </Link>
              ))}
            </div>
          </section>
        )}
      </div>
    </aside>
  );
}
