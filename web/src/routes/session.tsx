// One session (DESIGN.md §3.1): its header and actions, then the
// structured timeline or the terminal.

import { useNavigate } from "@tanstack/react-router";
import { useEffect, useState, type ReactElement } from "react";

import { CallFailed } from "../bridge/client";
import { useCrumbs, useFleet, useFleetState } from "../context";
import {
  harnessName,
  isLive,
  shortId,
  statusTone,
  statusWord,
} from "../data/format";
import { labelOf } from "../data/nodes";
import { useWhere } from "../data/where";
import { Dialog } from "../ui/dialog";
import {
  Badge,
  Banner,
  CopyButton,
  Icon,
  Skeleton,
  StatusDot,
} from "../ui/parts";
import { toast } from "../ui/toasts";
import type { SessionInfo } from "../wire/proto";
import { StructuredView } from "./structured";
import { TerminalView } from "./terminal";

export function SessionPage(): ReactElement {
  const where = useWhere();
  const node = where.segments[1] ?? "";
  const id = where.segments[2] ?? "";
  const fleet = useFleet();
  const label = useFleetState((state) => labelOf(state, node));
  const listed = useFleetState((state) =>
    state.sessions[node]?.find((session) => session.id === id),
  );
  const [fetched, setFetched] = useState<SessionInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  useCrumbs(
    [
      { label: "Fleet", to: "/fleet" },
      { label, to: `/fleet/${node}` },
      { label: shortId(id), mono: true },
    ],
    `${node}/${id}/${label}`,
  );
  useEffect(() => {
    let current = true;
    fleet.bridge.call(node, "session.get", { session: id }).then(
      (info) => {
        if (current) {
          setFetched(info);
        }
      },
      (failure: unknown) => {
        if (current) {
          setError(
            failure instanceof Error ? failure.message : String(failure),
          );
        }
      },
    );
    return () => {
      current = false;
    };
  }, [fleet, node, id]);
  // The list is fresher (polled); the first read covers a session not in
  // it yet.
  const session = listed ?? fetched;
  if (session === null) {
    return error === null ? (
      <div className="page">
        <Skeleton />
      </div>
    ) : (
      <div className="page">
        <Banner tone="error">
          Session {shortId(id)} on {label}: {error}
        </Banner>
      </div>
    );
  }
  const structured = session.spec.mode === "structured";
  return (
    <div className={structured ? "session" : "session term"}>
      <SessionHead node={node} label={label} session={session} />
      {structured ? (
        <StructuredView
          key={session.id}
          node={node}
          label={label}
          session={session}
        />
      ) : (
        <TerminalView
          key={session.id}
          node={node}
          label={label}
          session={session}
        />
      )}
    </div>
  );
}

function SessionHead({
  node,
  label,
  session,
}: {
  node: string;
  label: string;
  session: SessionInfo;
}) {
  const fleet = useFleet();
  const navigate = useNavigate();
  const [dialog, setDialog] = useState<"kill" | "profile" | null>(null);
  const live = isLive(session);
  const tone = statusTone(session.status);
  const profile = session.profile ?? harnessName(session.spec.harness);
  return (
    <header className="session-head">
      <StatusDot tone={tone} label={statusWord(session.status)} />
      <span className="id">{shortId(session.id)}</span>
      <Badge tone={tone}>{statusWord(session.status)}</Badge>
      <Badge tone={session.spec.mode === "structured" ? "accent" : ""}>
        {session.spec.mode}
      </Badge>
      <span className="meta">
        {label} · {profile}
        {session.model === undefined || session.model === null
          ? ""
          : ` · ${session.model.route} (${session.model.model})`}{" "}
        · {session.spec.cwd ?? "~"}
      </span>
      <div className="actions">
        {session.spec.mode === "structured" && live ? (
          <button
            className="btn sm"
            type="button"
            onClick={() => {
              setDialog("profile");
            }}
          >
            <Icon name="move" size="sm" /> Profile
          </button>
        ) : null}
        <CopyButton text={session.id} label="Copy session id" />
        {live ? (
          <button
            className="btn sm danger"
            type="button"
            onClick={() => {
              setDialog("kill");
            }}
          >
            <Icon name="kill" size="sm" /> Kill
          </button>
        ) : null}
      </div>
      {dialog === "kill" ? (
        <Dialog
          title={`Kill ${shortId(session.id)}?`}
          onClose={() => {
            setDialog(null);
          }}
          footer={
            <>
              <button
                className="btn ghost"
                type="button"
                onClick={() => {
                  setDialog(null);
                }}
              >
                Cancel
              </button>
              <button
                className="btn danger solid"
                type="button"
                onClick={() => {
                  setDialog(null);
                  fleet.bridge
                    .call(node, "session.kill", { session: session.id })
                    .then(
                      () => {
                        toast(`Killed ${shortId(session.id)}`);
                        void fleet.refreshSessions(node);
                      },
                      (failure: unknown) => {
                        toast(
                          failure instanceof Error
                            ? failure.message
                            : String(failure),
                          "error",
                        );
                      },
                    );
                }}
              >
                Kill
              </button>
            </>
          }
        >
          <p className="hint">
            The harness on {label} and everything it started stop now. The
            session&apos;s log stays.
          </p>
        </Dialog>
      ) : null}
      {dialog === "profile" ? (
        <MoveProfile
          node={node}
          session={session}
          close={() => {
            setDialog(null);
          }}
          moved={() => {
            setDialog(null);
            void fleet.refreshSessions(node);
            void navigate({
              to: "/sessions/$node/$id",
              params: { node, id: session.id },
            });
          }}
        />
      ) : null}
    </header>
  );
}

function MoveProfile({
  node,
  session,
  close,
  moved,
}: {
  node: string;
  session: SessionInfo;
  close: () => void;
  moved: () => void;
}) {
  const fleet = useFleet();
  const profiles = useFleetState((state) => state.profiles[node]);
  const choices = (profiles ?? []).filter(
    (profile) => profile.harness === session.spec.harness,
  );
  // A session keeps its model route: the node refuses a move to a
  // profile of another route, or between a route and none (ADR 0015).
  const route = session.model?.route ?? null;
  const [chosen, setChosen] = useState<string>(
    route === null
      ? ""
      : (choices.find(
          (profile) =>
            profile.model === route && profile.name !== session.profile,
        )?.name ??
          session.profile ??
          ""),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <Dialog
      title="Move to another profile"
      onClose={close}
      footer={
        <>
          <button className="btn ghost" type="button" onClick={close}>
            Cancel
          </button>
          <button
            className="btn primary"
            type="button"
            disabled={busy}
            onClick={() => {
              setBusy(true);
              fleet.bridge
                .call(node, "session.move_profile", {
                  session: session.id,
                  profile: chosen === "" ? null : chosen,
                })
                .then(
                  () => {
                    toast(
                      `Moved ${shortId(session.id)} to ${chosen === "" ? "the default profile" : chosen}`,
                    );
                    moved();
                  },
                  (failure: unknown) => {
                    setBusy(false);
                    setError(
                      failure instanceof CallFailed
                        ? failure.message
                        : String(failure),
                    );
                  },
                );
            }}
          >
            Move
          </button>
        </>
      }
    >
      <p className="hint">
        The conversation continues under the other account, between turns. Only
        profiles of {harnessName(session.spec.harness)} on this node are offered
        {route === null
          ? "."
          : `, on route ${route}: a session keeps the route it started on.`}
      </p>
      <label className="field">
        <span>Profile</span>
        <select
          value={chosen}
          onChange={(event) => {
            setChosen(event.target.value);
          }}
        >
          <option value="" disabled={route !== null}>
            default
          </option>
          {choices.map((profile) => (
            <option
              key={profile.name}
              value={profile.name}
              disabled={(profile.model ?? null) !== route}
            >
              {profile.label ?? profile.name}
              {profile.model === undefined || profile.model === null
                ? ""
                : ` · ${profile.model}`}
            </option>
          ))}
        </select>
      </label>
      {error === null ? null : <p className="field-error">{error}</p>}
    </Dialog>
  );
}
