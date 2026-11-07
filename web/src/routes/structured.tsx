// A structured session (DESIGN.md §6.5–6.7): the timeline, the inspector
// (plan, files, meta) and the composer with any pending permission.

import { type SyntheticEvent, useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";

import { useFleet } from "../context";
import { harnessName, isLive, shortId, when } from "../data/format";
import {
  blockText,
  EMPTY,
  type Entry,
  reduce,
  type Timeline,
  type ToolState,
} from "../data/timeline";
import { Badge, Banner, Icon, type IconName } from "../ui/parts";
import { toast } from "../ui/toasts";
import type { StreamState } from "../wire/bridge";
import type {
  EventRecord,
  PermissionOption,
  SessionInfo,
  ToolCallStatus,
  ToolKind,
} from "../wire/proto";

/** The session's timeline, following it from the start; records are
 * folded in once per frame, however fast they come. */
function useTimeline(
  node: string,
  session: string,
): { timeline: Timeline; stream: StreamState | null } {
  const fleet = useFleet();
  const [timeline, setTimeline] = useState<Timeline>(EMPTY);
  const [stream, setStream] = useState<StreamState | null>(null);
  useEffect(() => {
    let batch: EventRecord[] = [];
    let frame: number | null = null;
    const flush = () => {
      frame = null;
      const records = batch;
      batch = [];
      setTimeline((current) => reduce(current, records));
    };
    const stop = fleet.bridge.follow(node, session, 0, {
      record: (record) => {
        batch.push(record);
        frame ??= window.requestAnimationFrame(flush);
      },
      state: (state) => {
        setStream(state);
      },
    });
    return () => {
      stop();
      if (frame !== null) {
        window.cancelAnimationFrame(frame);
      }
    };
  }, [fleet, node, session]);
  return { timeline, stream };
}

export function StructuredView({
  node,
  label,
  session,
}: {
  node: string;
  label: string;
  session: SessionInfo;
}) {
  const { timeline, stream } = useTimeline(node, session.id);
  const end = useRef<HTMLDivElement>(null);
  const count = timeline.entries.length;
  useEffect(() => {
    end.current?.scrollIntoView({ block: "end" });
  }, [count]);
  return (
    <>
      {stream?.state === "reconnecting" ? (
        <Banner>
          Reconnecting to {label} · {stream.message}
        </Banner>
      ) : stream?.state === "failed" ? (
        <Banner tone="error">The stream stopped: {stream.message}</Banner>
      ) : null}
      <div className="timeline" aria-live="polite">
        {timeline.entries.map((entry) => (
          <TimelineItem
            key={entry.key}
            entry={entry}
            node={node}
            session={session}
          />
        ))}
        {count === 0 ? (
          <p className="t-meta">Waiting for the session&apos;s first record…</p>
        ) : null}
        <div ref={end} />
      </div>
      <Inspector label={label} session={session} timeline={timeline} />
      <Compose
        node={node}
        label={label}
        session={session}
        timeline={timeline}
      />
    </>
  );
}

function toolIcon(kind: ToolKind): IconName {
  switch (kind) {
    case "read":
      return "read";
    case "edit":
    case "delete":
      return "edit";
    case "move":
      return "move";
    case "search":
      return "search-kind";
    case "execute":
      return "exec";
    case "think":
      return "plan";
    case "fetch":
      return "link";
    case "switch_mode":
    case "other":
      return "more";
  }
}

function toolTone(status: ToolCallStatus): string {
  switch (status) {
    case "pending":
    case "in_progress":
      return "warn";
    case "completed":
      return "applied";
    case "failed":
      return "danger";
  }
}

function json(value: unknown): string {
  return typeof value === "string" ? value : JSON.stringify(value, null, 2);
}

function ToolCard({ tool }: { tool: ToolState }) {
  const open = tool.status === "pending" || tool.status === "in_progress";
  const path = tool.locations[0]?.path;
  return (
    <details className="tool" open={open}>
      <summary>
        <Icon name={toolIcon(tool.kind)} size="sm" />
        <span className="title">{tool.title}</span>
        <Badge tone={toolTone(tool.status)}>
          {tool.status.replace("_", " ")}
        </Badge>
        {path === undefined ? null : (
          <span className="loc" title={path}>
            <bdi>{path}</bdi>
          </span>
        )}
      </summary>
      <div className="body">
        {tool.input === null ? null : (
          <>
            <div className="io-label">Input</div>
            <pre>{json(tool.input)}</pre>
          </>
        )}
        {tool.content.map((content, index) =>
          content.type === "diff" ? (
            <div key={index}>
              <div className="io-label">Diff · {content.path}</div>
              <div
                className="diff"
                role="figure"
                aria-label={`Diff of ${content.path}`}
              >
                {(content.old_text ?? "").split("\n").map((line, at) =>
                  line === "" ? null : (
                    <div className="ln del" key={`d${at}`}>
                      <span className="n">{at + 1}</span>
                      <span className="n" />
                      <span className="tx">- {line}</span>
                    </div>
                  ),
                )}
                {content.new_text.split("\n").map((line, at) => (
                  <div className="ln add" key={`a${at}`}>
                    <span className="n" />
                    <span className="n">{at + 1}</span>
                    <span className="tx">+ {line}</span>
                  </div>
                ))}
              </div>
            </div>
          ) : (
            <div key={index}>
              <div className="io-label">Content</div>
              <pre>{blockText(content.content)}</pre>
            </div>
          ),
        )}
        {tool.output === null ? null : (
          <>
            <div className="io-label">Output</div>
            <pre>{json(tool.output)}</pre>
          </>
        )}
      </div>
    </details>
  );
}

function answerTone(option: PermissionOption, first: boolean): string {
  if (option.kind === "reject_always") {
    return "btn sm danger";
  }
  return first ? "btn primary sm" : "btn sm";
}

/** Answer a permission request; the first answer from any follower wins. */
function useAnswer(node: string, session: SessionInfo) {
  const fleet = useFleet();
  return (permission: string, option: string) => {
    fleet.bridge
      .call(node, "session.send", {
        session: session.id,
        input: {
          type: "answer_permission",
          id: permission,
          outcome: { outcome: "selected", option },
        },
      })
      .catch((failure: unknown) => {
        toast(
          failure instanceof Error ? failure.message : String(failure),
          "error",
        );
      });
  };
}

function PermissionCard({
  entry,
  node,
  session,
}: {
  entry: Extract<Entry, { kind: "permission" }>;
  node: string;
  session: SessionInfo;
}) {
  const answer = useAnswer(node, session);
  const chosen =
    entry.outcome === null
      ? null
      : entry.outcome.outcome === "cancelled"
        ? "cancelled"
        : (entry.request.options.find((option) =>
            entry.outcome?.outcome === "selected"
              ? option.id === entry.outcome.option
              : false,
          )?.name ?? "answered");
  return (
    <div className="perm-card">
      <h3>{chosen === null ? "Permission" : `Permission · ${chosen}`}</h3>
      <p>{entry.title}</p>
      {chosen === null ? (
        <div className="perm-actions">
          {entry.request.options.map((option, index) => (
            <button
              key={option.id}
              className={answerTone(option, index === 0)}
              type="button"
              onClick={() => {
                answer(entry.request.id, option.id);
              }}
            >
              {option.name}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

function TimelineItem({
  entry,
  node,
  session,
}: {
  entry: Entry;
  node: string;
  session: SessionInfo;
}) {
  switch (entry.kind) {
    case "ready":
      return (
        <div className="t-item meta-item">
          <span className="glyph" aria-hidden="true" />
          <p className="t-meta">
            Ready · harness session <code>{entry.harnessSession}</code>
          </p>
        </div>
      );
    case "prompt":
      return (
        <div className="t-item user">
          <span className="glyph" aria-hidden="true" />
          <span className="when">
            {entry.replayed ? "replayed" : `turn ${entry.turn}`}
          </span>
          <div className="prompt">{entry.text}</div>
        </div>
      );
    case "message":
      return (
        <div className="t-item">
          <span className="glyph" aria-hidden="true" />
          <div className="md">
            <ReactMarkdown skipHtml>{entry.text}</ReactMarkdown>
          </div>
        </div>
      );
    case "thought":
      return (
        <div className="t-item">
          <span className="glyph" aria-hidden="true" />
          <details className="thought">
            <summary>Thought</summary>
            <p>{entry.text}</p>
          </details>
        </div>
      );
    case "tool":
      return (
        <div className="t-item tool">
          <span className="glyph" aria-hidden="true" />
          <ToolCard tool={entry.tool} />
        </div>
      );
    case "plan": {
      const done = entry.entries.filter(
        (item) => item.status === "completed",
      ).length;
      return (
        <div className="t-item meta-item">
          <span className="glyph" aria-hidden="true" />
          <p className="t-meta">
            Plan · {done} of {entry.entries.length} done
          </p>
        </div>
      );
    }
    case "permission":
      return (
        <div className="t-item perm">
          <span className="glyph" aria-hidden="true" />
          <PermissionCard entry={entry} node={node} session={session} />
        </div>
      );
    case "turn_ended":
      return (
        <div className="t-item meta-item">
          <span className="glyph" aria-hidden="true" />
          <p className="t-meta">
            Turn ended · {entry.reason.replace("_", " ")}
          </p>
        </div>
      );
    case "turn_failed":
      return (
        <div className="t-item meta-item">
          <span className="glyph" aria-hidden="true" />
          <p className="t-meta error">Turn failed · {entry.message}</p>
        </div>
      );
    case "log":
      return (
        <div className="t-item meta-item">
          <span className="glyph" aria-hidden="true" />
          <p className="t-meta">
            <code>{entry.line}</code>
          </p>
        </div>
      );
    case "status":
      return (
        <div className="t-item meta-item">
          <span className="glyph" aria-hidden="true" />
          <p className="t-meta">
            Session {entry.status.state} · {when(entry.at)}
          </p>
        </div>
      );
    case "profile":
      return (
        <div className="t-item meta-item">
          <span className="glyph" aria-hidden="true" />
          <p className="t-meta">
            Profile changed from {entry.from ?? "default"} to{" "}
            {entry.to ?? "default"}
          </p>
        </div>
      );
  }
}

function Inspector({
  label,
  session,
  timeline,
}: {
  label: string;
  session: SessionInfo;
  timeline: Timeline;
}) {
  const spec = session.spec;
  return (
    <aside
      className="inspect"
      role="complementary"
      aria-label="Session inspector"
    >
      <section>
        <h2>Plan</h2>
        {timeline.plan === null ? (
          <p className="hint">No plan yet.</p>
        ) : (
          timeline.plan.map((item, index) => (
            <div
              key={index}
              className={
                item.status === "completed"
                  ? "plan-item done"
                  : item.status === "in_progress"
                    ? "plan-item now"
                    : "plan-item"
              }
            >
              <span className="mark-box" aria-hidden="true" />
              <span>{item.content}</span>
            </div>
          ))
        )}
      </section>
      <section>
        <h2>Files</h2>
        {timeline.files.length === 0 ? (
          <p className="hint">No files touched yet.</p>
        ) : (
          timeline.files.map((path) => (
            <div key={path} className="file-row" title={path}>
              <Icon name="read" size="sm" />
              {path.split("/").at(-1)}
            </div>
          ))
        )}
      </section>
      <section>
        <h2>Meta</h2>
        <dl>
          <dt>id</dt>
          <dd>{session.id}</dd>
          <dt>node</dt>
          <dd>{label}</dd>
          <dt>harness</dt>
          <dd>{harnessName(spec.harness)}</dd>
          <dt>profile</dt>
          <dd>{session.profile ?? "default"}</dd>
          <dt>model</dt>
          <dd>
            {spec.mode === "structured"
              ? (spec.model ?? "harness default")
              : "—"}
          </dd>
          <dt>cwd</dt>
          <dd>{spec.cwd ?? "~"}</dd>
          <dt>created</dt>
          <dd>{when(session.created_at)}</dd>
          <dt>by</dt>
          <dd>
            {session.created_by.kind === "agent"
              ? `agent of ${shortId(session.created_by.session)}`
              : session.created_by.kind === "device"
                ? "a paired device"
                : "you"}
          </dd>
          <dt>seq</dt>
          <dd>{timeline.next}</dd>
        </dl>
      </section>
    </aside>
  );
}

function Compose({
  node,
  label,
  session,
  timeline,
}: {
  node: string;
  label: string;
  session: SessionInfo;
  timeline: Timeline;
}) {
  const fleet = useFleet();
  const answer = useAnswer(node, session);
  const [text, setText] = useState("");
  const [sending, setSending] = useState(false);
  const pending = timeline.pending;
  const live = isLive(session);
  const blocked = pending !== null || !live || sending;
  const send = (event?: SyntheticEvent) => {
    event?.preventDefault();
    const prompt = text.trim();
    if (prompt === "" || blocked) {
      return;
    }
    setSending(true);
    fleet.bridge
      .call(node, "session.send", {
        session: session.id,
        input: { type: "prompt", content: [{ type: "text", text: prompt }] },
      })
      .then(
        () => {
          setText("");
          setSending(false);
        },
        (failure: unknown) => {
          setSending(false);
          toast(
            failure instanceof Error ? failure.message : String(failure),
            "error",
          );
        },
      );
  };
  const spec = session.spec;
  const model =
    spec.mode === "structured" ? (spec.model ?? "harness default") : "";
  return (
    <div className="compose">
      {pending === null ? null : (
        <div className="perm-sticky perm-card" aria-live="assertive">
          <h3>Waiting on permission</h3>
          <p>{pending.title} · answer above or here. The composer is paused.</p>
          <div className="perm-actions">
            {pending.request.options.map((option, index) => (
              <button
                key={option.id}
                className={answerTone(option, index === 0)}
                type="button"
                onClick={() => {
                  answer(pending.request.id, option.id);
                }}
              >
                {option.name}
              </button>
            ))}
          </div>
        </div>
      )}
      <form className="composer" aria-label="Prompt" onSubmit={send}>
        <textarea
          aria-label="Prompt"
          disabled={blocked}
          placeholder={
            pending !== null
              ? "Answer the permission to continue"
              : live
                ? "Ask, or describe the change · ⌘↵ to send"
                : "The session has ended"
          }
          value={text}
          onChange={(event) => {
            setText(event.target.value);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
              send();
            }
          }}
        />
        <div className="composer-bar">
          <span className="model">{model}</span>
          <span>{session.profile ?? harnessName(spec.harness)}</span>
          <span>
            turn {timeline.turns} ·{" "}
            {pending !== null
              ? "waiting"
              : timeline.inTurn
                ? "working"
                : "idle"}
          </span>
          {timeline.inTurn ? (
            <button
              className="btn sm"
              type="button"
              onClick={() => {
                void fleet.bridge.call(node, "session.send", {
                  session: session.id,
                  input: { type: "cancel_turn" },
                });
              }}
            >
              Stop
            </button>
          ) : null}
          <button
            className="btn primary sm"
            type="submit"
            disabled={blocked || text.trim() === ""}
          >
            Send <kbd className="kbd">⌘↵</kbd>
          </button>
        </div>
      </form>
      <div className="status-line">
        <span>{label} · via vessel ui</span>
        <span>seq {timeline.next}</span>
      </div>
    </div>
  );
}
