# Migration guide

Vessel has no tagged release yet, so there are no released version-to-version
migration instructions. The workspace version is `0.1.0`. Development
checkouts can change; consult the [changelog](../CHANGELOG.md),
[protocol history](PROTOCOL.md) and relevant [ADRs](adr/) when comparing
commits.

For a node reached over SSH, the [README](../README.md#another-machine-over-ssh)
describes `vessel node update` and `--when-idle`. A node restart interrupts
live sessions; durable logs remain readable. No release compatibility or
rollback guarantee is established here.
