# Interfaces and reference

Vessel exposes a CLI, a typed Rust client and a wire protocol. This page
points to their authoritative references rather than repeating their contracts.

## Command line

Run `vessel --help` and `vessel <command> --help` for commands and options.
The [README](../README.md) covers local sessions, SSH, pairing, the relay,
profiles and fleets; the [tutorial](tutorial.md) walks through a local session.

## Rust client and wire protocol

The `vessel-client` crate provides the typed client and `vessel-proto` owns the
wire request and response types. The source and Rust documentation on their
public items define their interfaces. [PROTOCOL.md](PROTOCOL.md) describes
protocol versions, methods, permissions and transport behavior;
[ARCHITECTURE.md](ARCHITECTURE.md) describes the crate boundaries.

## Agent tools

A node starts `vessel mcp` for a session's agent. The
[README's agent tools section](../README.md#agent-tools) lists its tools and
scope limits; [ADR 0013](adr/0013-agent-principals.md) describes the identity
and security model.
