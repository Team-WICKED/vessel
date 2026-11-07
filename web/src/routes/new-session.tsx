// New session (DESIGN.md §7.5): each field filters the next. The dialog
// stays open until the node answers; a failure shows inline.

import { useNavigate } from "@tanstack/react-router";
import { type SyntheticEvent, useState } from "react";

import { closeNewSession, useFleet, useFleetState } from "../context";
import { harnessName, nodeState } from "../data/format";
import { nodeLabel } from "../data/nodes";
import { Dialog } from "../ui/dialog";
import { Icon } from "../ui/parts";
import type { HarnessKind, SessionSpec } from "../wire/proto";

type Mode = "structured" | "terminal";

export function NewSessionDialog({
  node: preselected,
}: {
  node: string | null;
}) {
  const fleet = useFleet();
  const navigate = useNavigate();
  const book = useFleetState((state) => state.book);
  const statuses = useFleetState((state) => state.statuses);
  const allHarnesses = useFleetState((state) => state.harnesses);
  const allProfiles = useFleetState((state) => state.profiles);
  const nodes = book ?? [];
  const connected = nodes.filter(
    (node) => statuses[node.key]?.state === "connected",
  );
  const [node, setNode] = useState<string>(
    preselected !== null &&
      connected.some((candidate) => candidate.key === preselected)
      ? preselected
      : ((connected.find((candidate) => candidate.default) ?? connected[0])
          ?.key ?? ""),
  );
  const harnesses = allHarnesses[node] ?? [];
  const [harness, setHarness] = useState<HarnessKind | "">(
    harnesses[0]?.kind ?? "",
  );
  const chosenHarness =
    harnesses.find((candidate) => candidate.kind === harness) ?? harnesses[0];
  const structuredAvailable = chosenHarness?.structured.state === "available";
  const profiles = (allProfiles[node] ?? []).filter(
    (profile) => profile.harness === chosenHarness?.kind,
  );
  const [profile, setProfile] = useState("");
  const [model, setModel] = useState("");
  const [cwd, setCwd] = useState("");
  const [mode, setMode] = useState<Mode>("structured");
  const effectiveMode: Mode = structuredAvailable ? mode : "terminal";
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const create = (event?: SyntheticEvent) => {
    event?.preventDefault();
    if (chosenHarness === undefined || busy) {
      return;
    }
    const spec: SessionSpec =
      effectiveMode === "structured"
        ? {
            harness: chosenHarness.kind,
            mode: "structured",
            model: model === "" ? null : model,
          }
        : {
            harness: chosenHarness.kind,
            mode: "terminal",
            args: [],
            size: { cols: 120, rows: 36 },
          };
    if (cwd.trim() !== "") {
      spec.cwd = cwd.trim();
    }
    if (profile !== "") {
      spec.profile = profile;
    }
    setBusy(true);
    setError(null);
    fleet.bridge.call(node, "session.create", { spec }).then(
      (info) => {
        closeNewSession();
        void fleet.refreshSessions(node);
        void navigate({
          to: "/sessions/$node/$id",
          params: { node, id: info.id },
        });
      },
      (failure: unknown) => {
        setBusy(false);
        setError(failure instanceof Error ? failure.message : String(failure));
      },
    );
  };

  return (
    <Dialog
      title="New session"
      wide
      onClose={closeNewSession}
      footer={
        <>
          <button className="btn ghost" type="button" onClick={closeNewSession}>
            Cancel
          </button>
          <button
            className="btn primary"
            type="submit"
            form="new-session"
            disabled={busy || chosenHarness === undefined}
          >
            {busy ? "Creating…" : "Create"}
          </button>
        </>
      }
    >
      <form id="new-session" onSubmit={create}>
        <p className="hint">
          Each field filters the next. Nodes that are not connected cannot take
          sessions.
        </p>
        <div className="grid-2">
          <label className="field">
            <span>Node</span>
            <select
              aria-label="Node"
              value={node}
              onChange={(event) => {
                setNode(event.target.value);
                setHarness("");
                setProfile("");
              }}
            >
              {nodes.map((candidate) => {
                const status = statuses[candidate.key];
                return (
                  <option
                    key={candidate.key}
                    value={candidate.key}
                    disabled={status?.state !== "connected"}
                  >
                    {nodeLabel(candidate, status)} · {nodeState(status).word}
                  </option>
                );
              })}
            </select>
          </label>
          <label className="field">
            <span>Harness</span>
            <select
              aria-label="Harness"
              value={chosenHarness?.kind ?? ""}
              onChange={(event) => {
                setHarness(event.target.value as HarnessKind);
                setProfile("");
              }}
            >
              {harnesses.length === 0 ? (
                <option value="">no harness found</option>
              ) : null}
              {harnesses.map((candidate) => (
                <option key={candidate.kind} value={candidate.kind}>
                  {harnessName(candidate.kind)}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>Profile</span>
            <select
              aria-label="Profile"
              value={profile}
              onChange={(event) => {
                setProfile(event.target.value);
              }}
            >
              <option value="">default</option>
              {profiles.map((candidate) => (
                <option key={candidate.name} value={candidate.name}>
                  {candidate.label ?? candidate.name}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>Model</span>
            <select
              aria-label="Model"
              value={model}
              disabled={effectiveMode !== "structured"}
              onChange={(event) => {
                setModel(event.target.value);
              }}
            >
              <optgroup label="Node default">
                <option value="">harness default</option>
              </optgroup>
            </select>
            <span className="hint">
              Routed models arrive with model routing (M5).
            </span>
          </label>
        </div>
        <label className="field">
          <span>Working directory</span>
          <input
            aria-label="Working directory"
            type="text"
            spellCheck={false}
            placeholder="the node's home directory"
            value={cwd}
            onChange={(event) => {
              setCwd(event.target.value);
            }}
          />
        </label>
        <div className="field">
          <span>Mode</span>
          <div className="seg" role="radiogroup" aria-label="Mode">
            <label
              className={effectiveMode === "structured" ? "active" : undefined}
            >
              <input
                type="radio"
                name="mode"
                checked={effectiveMode === "structured"}
                disabled={!structuredAvailable}
                onChange={() => {
                  setMode("structured");
                }}
              />
              <span>
                <Icon name="message" size="sm" /> Structured
              </span>
            </label>
            <label
              className={effectiveMode === "terminal" ? "active" : undefined}
            >
              <input
                type="radio"
                name="mode"
                checked={effectiveMode === "terminal"}
                onChange={() => {
                  setMode("terminal");
                }}
              />
              <span>
                <Icon name="terminal" size="sm" /> Terminal
              </span>
            </label>
          </div>
          <p className="hint">
            {chosenHarness?.structured.state === "unavailable"
              ? `Structured is unavailable: ${chosenHarness.structured.reason}`
              : "Structured is a timeline (ACP). Terminal is the harness TUI in a PTY."}
          </p>
        </div>
        {error === null ? null : (
          <p className="field-error" role="alert">
            {error}
          </p>
        )}
      </form>
    </Dialog>
  );
}
