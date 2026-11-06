# ADR 0004: Durable per-session event log in SQLite

Status: accepted, 2026-11-05

## Context

Clients attach late, disconnect, and reconnect, and the node restarts. Phase 0
kept 2000 lines in memory per session and lost everything on restart.
Paseo's timeline is in memory with a random epoch per run, so a restart
invalidates every client cursor and forces a full re-read of each provider's
transcript.

## Options

1. In-memory ring only. Simple; loses history on restart.
2. Append-only JSONL file per session. No extra dependency, but paging
   backwards, retention and crash-safe metadata updates are hand-written, and
   there are thousands of files.
3. SQLite (one database, WAL mode) holding session metadata and event rows.

## Decision

Option 3, owned by `vessel-store`:

- Table `sessions`: id, spec, status, created/ended times, next seq, harness
  session id. Table `events`: `(session_id, seq)` primary key, recorded time,
  the `SessionEvent` as JSON text.
- One writer thread receives batches over a bounded channel and commits per
  batch (no `fsync` per event). Reads use separate connections on the
  blocking pool.
- `EventSeq` is assigned by the session runtime, gap-free from zero, and is
  durable, so `(SessionId, EventSeq)` is a cursor that survives node restarts.
  There is no epoch.
- On startup, sessions that were live are marked `Interrupted`.
- Retention: a byte cap per session and a cap on the number of ended
  sessions; the oldest are pruned first. `session.read` reports the oldest
  retained seq. (Amended 2026-11-06: for a live session, records pruned
  from the store but still in its ring are read from the ring, so a
  catching-up follower misses only what the node no longer has at all.)
- Schema version in `PRAGMA user_version`, with forward-only migrations.

## Why

- Paged reads by seq and pruning are single indexed queries.
- One file, transactional, inspectable with the `sqlite3` CLI.
- The writer thread keeps blocking I/O off the async runtime.

## Consequences

- Adds `rusqlite` with the bundled SQLite (a C build step, which CI already
  handles on all three OSes).
- Terminal output is stored base64-encoded inside JSON. Revisit with a BLOB
  column if terminal-heavy logs get large.
- The in-memory ring in `vessel-session` is only a cache for live subscribers;
  the store is the source of truth for history.
