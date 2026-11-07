# ADR 0005: Every connection presents the node token

Status: accepted, 2026-11-05; amended 2026-11-05 (no TLS for token
connections, below). Extended by ADR 0010 (paired devices) and ADR 0013
(agents); `wss://` exists only for relays and paired nodes (ADR 0009).

## Context

Phase 0 had no authentication and bound to loopback. Loopback is not a
security boundary on a shared machine, and Phase 1 exposes nodes to other
machines. Paseo uses one bearer token per daemon run and takes caller identity
from a query parameter, so any local agent can impersonate another.

## Options

1. No auth on loopback, a token otherwise. Convenient, but unsafe on
   multi-user hosts and easy to misconfigure.
2. A token on every connection, read from a 0600 file by local clients.
3. Unix socket permissions only. Strong locally, but no Windows story and no
   help for remote clients.

## Decision

Option 2:

- On first start the node writes a random 256-bit token to
  `<data_dir>/token` (mode 0600 on Unix). The token never appears in logs or
  in `NodeInfo`.
- The websocket upgrade must carry `Authorization: Bearer <token>`. The
  comparison takes constant time. `/health` stays open.
- Local clients (`vessel` CLI) read the token file; remote clients get it
  through `--token`/`VESSEL_TOKEN`. The CLI reads the file only when
  `--node` is a loopback address or `localhost` (decided from the text, no
  DNS). For any other node it requires the token explicitly, so this
  machine's token is never sent to another host in clear text (see the
  amendment below).
- Binding a non-loopback address logs a warning that names the token file.

## Why

- One rule everywhere, no "safe because local" assumption.
- Matches what the relay will need anyway: a principal per connection.

## Consequences

- Per-agent principals (so a harness calling back into the node is
  identified and limited) come in Phase 2, before agents get node tools.
- Token rotation is "delete the file and restart" for now.

## Amendment (2026-11-05): no TLS in milestone 1

Milestone 1 nodes are reached on loopback or through an SSH tunnel, so the
token travels over plain `ws://` only. The workspace builds no TLS stack:
`tokio-tungstenite` was configured with `rustls` but no crypto provider,
which would have panicked on the first `wss://` handshake.
`vessel-client` refuses `wss://` with `ConnectError::TlsUnsupported`
before any connection is attempted. TLS is chosen together with the relay
(milestone 3), with an explicit crypto provider.

