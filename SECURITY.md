# Security

## Supported versions

Vessel has no tagged release yet. The project has not published a supported
version or security-update policy; the workspace version is `0.1.0`.

## Reporting a vulnerability

Please do not open a public issue or disclose a suspected vulnerability in a
pull request. A private reporting channel has not yet been published, and
GitHub private vulnerability reporting is not currently enabled for this
repository. Team-WICKED needs to establish a private contact or enable that
feature before this document can offer a working reporting route.

Once a private route is available, a useful report includes the affected commit
or version, prerequisites, reproduction steps, potential impact and any
relevant sanitized logs. Never include active credentials or other secrets in
public reports.

## Deployment context

The [README](README.md) describes authentication, device pairing, relay
operation and proxy configuration. Agent identity provides attribution and
scoped access; it is not an operating-system sandbox. See
[ADR 0013](docs/adr/0013-agent-principals.md) and
[ADR 0009](docs/adr/0009-relay-and-transport-security.md) for their security
boundaries and limitations.
