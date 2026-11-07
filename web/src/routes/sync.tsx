// Sync (DESIGN.md §3.1): the fleet's configuration as its home publishes
// it — generation, files, MCP servers (secrets by name only) — then the
// members, with conflicts pinned above them.

import { useEffect, useState, type ReactElement } from "react";

import { useCrumbs, useFleet, useFleetState } from "../context";
import { ago } from "../data/format";
import { useWhere } from "../data/where";
import { labelOf, selectedNode } from "../data/nodes";
import { Badge, Empty, Skeleton } from "../ui/parts";
import type { FleetMember, Manifest, SecretName } from "../wire/proto";

interface Published {
  manifest: Manifest;
  members: readonly FleetMember[];
  secrets: readonly SecretName[];
}

function secretNames(manifest: Manifest): string[] {
  const names = new Set<string>();
  for (const transport of Object.values(manifest.mcp)) {
    const values =
      transport.type === "stdio"
        ? Object.values(transport.env ?? {})
        : Object.values(transport.headers ?? {});
    for (const value of values) {
      if (typeof value !== "string") {
        names.add(value.secret);
      }
    }
  }
  return [...names].sort();
}

export function SyncPage(): ReactElement {
  const where = useWhere();
  const fleet = useFleet();
  const node = useFleetState((state) => selectedNode(state, where.node));
  const label = useFleetState((state) =>
    node === null ? "" : labelOf(state, node),
  );
  const connected = useFleetState((state) =>
    node === null ? false : state.statuses[node]?.state === "connected",
  );
  // Results are kept with the node they are for, so switching nodes shows
  // nothing stale and needs no reset.
  const [result, setResult] = useState<{
    node: string;
    published: Published | null;
    error: string | null;
  } | null>(null);
  const published = result?.node === node ? result.published : null;
  const error = result?.node === node ? result.error : null;
  useCrumbs(
    [{ label: "Sync" }, ...(node === null ? [] : [{ label }])],
    `${node ?? ""}${label}`,
  );
  useEffect(() => {
    if (node === null || !connected) {
      return undefined;
    }
    let current = true;
    Promise.all([
      fleet.bridge.call(node, "fleet.manifest", null),
      fleet.bridge.call(node, "fleet.members", null),
      fleet.bridge.call(node, "secret.list", null),
    ]).then(
      ([manifest, members, secrets]) => {
        if (current) {
          setResult({
            node,
            published: { manifest, members, secrets },
            error: null,
          });
        }
      },
      (failure: unknown) => {
        if (current) {
          setResult({
            node,
            published: null,
            error: failure instanceof Error ? failure.message : String(failure),
          });
        }
      },
    );
    return () => {
      current = false;
    };
  }, [fleet, node, connected]);

  const head = (
    <div className="page-head">
      <div>
        <h1>Fleet sync</h1>
        <p>
          {published === null
            ? "What a fleet's home publishes: instructions, skills and MCP servers. Members pull it; local edits are kept."
            : `${label} is home, generation ${published.manifest.generation}. Members pull instructions, skills and MCP servers. Local edits are kept.`}
        </p>
      </div>
    </div>
  );
  if (node === null || !connected) {
    return (
      <div className="page">
        {head}
        <div className="page-body">
          <Empty
            title={
              node === null ? "No node selected" : `${label} is not connected`
            }
          >
            Sync shows what the selected node publishes as a fleet home.
          </Empty>
        </div>
      </div>
    );
  }
  if (error !== null) {
    return (
      <div className="page">
        {head}
        <div className="page-body">
          <Empty title={`${label} publishes no fleet configuration`}>
            {error}. Make it a home with <code>vessel fleet init</code>, or pick
            the home in the sidebar.
          </Empty>
        </div>
      </div>
    );
  }
  if (published === null) {
    return (
      <div className="page">
        {head}
        <div className="page-body">
          <Skeleton />
        </div>
      </div>
    );
  }
  const { manifest, members, secrets } = published;
  const files = Object.entries(manifest.files);
  const servers = Object.entries(manifest.mcp);
  const stale = members.filter(
    (member) => (member.applied ?? -1) < manifest.generation,
  ).length;
  const conflicts = members.filter(
    (member) => (member.conflicts ?? []).length > 0,
  );
  return (
    <div className="page">
      {head}
      <div className="page-body">
        <div className="strip">
          <div>
            <div className="k">Home</div>
            <div className="v">{label}</div>
            <div className="s">publishing</div>
          </div>
          <div>
            <div className="k">Generation</div>
            <div className="v mono">{manifest.generation}</div>
            <div className="s">
              {files.length} files · {servers.length} MCP servers
            </div>
          </div>
          <div>
            <div className="k">Members</div>
            <div className="v mono">{members.length}</div>
            <div className="s">
              {stale} stale · {conflicts.length} with conflicts
            </div>
          </div>
          <div>
            <div className="k">Secrets</div>
            <div className="v mono">{secrets.length}</div>
            <div className="s">by name only</div>
          </div>
        </div>
        <div className="panel">
          <h2>Published on {label}</h2>
          <div className="pad">
            {files.length === 0 ? (
              <p className="hint">No files published.</p>
            ) : null}
            {files.slice(0, 12).map(([path, file]) => (
              <div className="source-row" key={path}>
                <span className="k">File</span>
                <span className="path">{path}</span>
                <span className="muted mono">
                  {file.size} B{file.executable === true ? " · executable" : ""}
                </span>
              </div>
            ))}
            {servers.map(([name, transport]) => (
              <div className="source-row" key={name}>
                <span className="k">MCP</span>
                <span className="path">{name}</span>
                <span className="muted">
                  {transport.type === "stdio"
                    ? transport.command
                    : transport.url}
                </span>
              </div>
            ))}
            {secretNames(manifest).length === 0 ? null : (
              <div className="source-row">
                <span className="k">Secrets</span>
                <span className="path">{secretNames(manifest).join(", ")}</span>
                <span className="muted">resolved on each node</span>
              </div>
            )}
          </div>
        </div>
        {conflicts.map((member) => (
          <div className="conflict" key={member.device}>
            <h3>Conflict on {member.label}</h3>
            {(member.conflicts ?? []).map((path) => (
              <span className="path" key={path}>
                {path}
              </span>
            ))}
            <p className="hint">
              A local edit was kept. The home version is beside it as{" "}
              <span className="mono">.vessel-incoming</span>. Vessel never
              overwrites a file you changed.
            </p>
          </div>
        ))}
        {members.length === 0 ? (
          <Empty title={`No members have joined ${label}`}>
            Join one with <code>vessel fleet join</code> and a pairing link from
            this node.
          </Empty>
        ) : (
          <div className="table-wrap">
            <table className="data" aria-label="Members">
              <thead>
                <tr>
                  <th scope="col">Member</th>
                  <th scope="col">Applied</th>
                  <th scope="col">Lag</th>
                  <th scope="col">Conflicts</th>
                  <th scope="col">Last report</th>
                </tr>
              </thead>
              <tbody>
                {members.map((member) => {
                  const applied = member.applied ?? null;
                  const behind =
                    applied === null ? null : manifest.generation - applied;
                  return (
                    <tr key={member.device}>
                      <td className="name">{member.label}</td>
                      <td className="mono">{applied ?? "—"}</td>
                      <td>
                        {behind === null ? (
                          <Badge>not yet</Badge>
                        ) : behind === 0 ? (
                          <Badge tone="applied">current</Badge>
                        ) : (
                          <Badge tone="stale">{behind} behind</Badge>
                        )}
                      </td>
                      <td>
                        {(member.conflicts ?? []).length === 0 ? (
                          <span className="mono muted">0</span>
                        ) : (
                          <Badge tone="conflict">
                            {(member.conflicts ?? []).length}
                          </Badge>
                        )}
                      </td>
                      <td className="mono muted">
                        {member.reported_at === undefined ||
                        member.reported_at === null
                          ? "—"
                          : ago(member.reported_at)}
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
