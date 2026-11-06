# ADR 0014: A public repository under a source-available license

Status: accepted, 2026-11-06 (operator's decision)

## Context

CI runs the gate on Linux, Windows and macOS (AGENTS.md section 1). While
the repository was private, GitHub-hosted minutes counted against the
organisation's 2,000 a month, Windows at 2x and macOS at 10x. Running all
three on every push to `backend/**` used the whole month in two days
(2026-11-05 and 06: Linux 215, Windows 408 and macOS 107 minutes), and CI was
switched to manual runs.

Standard GitHub-hosted runners are free for public repositories. The
operator wants the free CI, and wants the license as strict as possible.

## Options

1. **Stay private.** Manual runs at milestone merges, or a self-hosted Linux
   runner (macOS still needs hosted minutes). Windows- and macOS-only breaks
   are found late.
2. **Public under an open source license.** AGPL-3.0 is the strictest
   common one, but it lets anyone use Vessel commercially, and running it
   only inside a company triggers none of its obligations.
3. **Public under a source-available license.** In order of strictness:
   BUSL-1.1 (no production use by others until a change date, then an open
   license), PolyForm Noncommercial 1.0.0 (any noncommercial use, changes
   and sharing allowed), PolyForm Strict 1.0.0 (noncommercial use only; no
   changes, no distribution).

## Decision

Option 3 with **PolyForm Strict 1.0.0**, the operator's choice. The license
text is in `LICENSE.md`, unchanged from the PolyForm project's 1.0.0 tag,
with Team-WICKED's copyright line above it. Every crate points at it with
`license-file` (Cargo has no SPDX id for it). Contributions from outside
Team-WICKED are not accepted.

Before the repository went public:

- The whole history was searched for credential patterns (GitHub, Anthropic,
  Slack and AWS tokens, private keys); none were found.
- The current tree of the active branches was cleaned of the operator's
  internal host names and private repository paths. Older commits keep them;
  they name machines and private repositories, not credentials, and
  rewriting the history of every branch would have broken the work in
  progress.

## Why

- CI on three OSes for every push again, at no cost.
- The strictest available terms: others may read and evaluate, and use it
  noncommercially, but may not build on it, ship it or sell it.

## Consequences

- Vessel is not open source. "Open source" is not to be used in its
  description.
- Team-WICKED, as the only copyright holder, can still license it on other
  terms (commercially, or more openly later). An outside contribution would
  change that, which is one reason none are accepted.
- Everything pushed is public, history and CI logs included: no credentials,
  customer data or internal addresses in commits, docs, tests or logs.
- If the repository ever goes private again, CI goes back to manual runs
  first (the `ci.yml` notes say how).
