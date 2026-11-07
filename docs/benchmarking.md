# Benchmarking

Vessel does not currently publish a benchmark suite or performance baseline.
The [roadmap](ROADMAP.md) and [architecture](ARCHITECTURE.md) describe the
implemented features; neither is a performance guarantee.

When reporting a measurement, make it reproducible: record the Vessel commit,
operating system, architecture, CPU and memory, Rust version, build profile,
configuration, connection type, harness and version if used, workload,
concurrency, sample count, warm-up, timing method, units and error rate.
Provide the exact commands and sanitized input, and distinguish a local run
from an SSH or relay run. Report the observed environment and results rather
than extrapolating to other installations.

Do not put tokens, credentials, customer data, private paths or internal
addresses in benchmark inputs or published results. Tests and manual smoke
scripts described in [CONTRIBUTING.md](../CONTRIBUTING.md) and
[docs/smoke/README.md](smoke/README.md) check behavior; they are not a
benchmark baseline.
