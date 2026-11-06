# ADR 0008: Installing and updating `vesseld` on a remote

Status: accepted, 2026-11-06, with the coordinator decisions in `docs/plans/backend-m2.md`. Drafted (milestone 2, `docs/plans/backend-m2.md`)

## Context

ADR 0007 runs `~/.vessel/bin/vesseld-<version> attach` over SSH, so that
binary has to be there. It has to match the client, and it has to be
replaceable without surprising anyone. ORIGIN item 1 names Zed's "fast
server-side update" as something to keep.

What Zed does (read 2026-11-06; ideas only):

- Binaries are named `zed-remote-server-<channel>-<version>` under
  `~/.zed_server`. A binary that answers `version` is used as it is.
- The remote downloads with `curl`, falling back to `wget`. If that fails,
  the client downloads and uploads with `sftp` or `scp`.
- The download is gunzipped, `chmod 755`ed and `mv`ed into place.
- When a server starts, it deletes older versions that no running process
  uses.
- There is no checksum or signature, and a new version simply kills the
  old server.

What Vessel has today:

- A node restart ends live sessions as `Interrupted`. Structured sessions
  can continue through ACP `session/load` (`vessel run --structured
  --resume`); terminal sessions cannot, because their PTY died with the
  node.
- The protocol negotiates a version range in `Hello`/`Welcome`, so a client
  and a node of different versions work together while their ranges
  overlap.
- No release artefacts exist yet. The repository is `Team-WICKED/vessel` on
  GitHub.

## Options

### Where binaries come from

1. **GitHub Releases** on the repository: per-target archives and a
   checksum file.
   - Free and served from a CDN, with stable download URLs that need no
     API call.
   - A private repository would need a token to download.
2. **Our own download server:** full control and analytics, but something
   to run, pay for and secure.
3. **Build on the remote** (`cargo install`): needs a toolchain and minutes
   of CPU on every machine.
4. **Upload only:** works offline and for development, but needs a local
   binary for the remote's target, which means a cross build when the
   machines differ.

### Integrity

1. None (Zed): HTTPS only.
2. A `SHA256SUMS` file from the same release. This catches corruption, but
   not a compromised release or account.
3. `SHA256SUMS` signed with a key whose public half is compiled into
   `vessel`. This also catches a replaced artefact, at the cost of keeping a
   signing key safe in CI.

### Live sessions during an update

1. **Kill the old node** (Zed). Every live session is interrupted without
   warning.
2. **Refuse while sessions are live.** Safe, but a busy node never updates.
3. **Drain.** The node refuses new sessions and exits when the last one
   ends; the next `attach` starts the new version.
4. **Explicit "now".** As option 1, but only when the user asks.
5. **A session host that outlives the node.** PTYs and agent processes live
   in a small, rarely changing process, and the node reconnects to it after
   an update, as tmux keeps panes alive across client restarts. This is the
   only option that updates without interrupting anything. It is also a new
   process boundary with its own protocol, and it fixes the session host's
   own version for a long time.

## Decision

- **Layout.** Versions sit side by side in `<data_dir>/bin/`:
  - `vesseld-<version>` for release builds;
  - `vesseld-<version>-dev-<first 16 hex of SHA-256>` for uploaded local
    builds. These are named by content, so two different builds that share
    a version never collide, and no build script is needed.
  - The client always runs the exact file for its own version. Nothing is
    called `vesseld` or `current`, so there is no link to switch, and
    Windows needs no symlink.
- **Source.** GitHub Releases of `Team-WICKED/vessel`, built by a release
  workflow on `v*` tags:
  - one `vesseld-<version>-<target>.tar.gz` per target, for Linux
    `x86_64`/`aarch64` (static musl) and macOS `x86_64`/`aarch64`;
  - a `SHA256SUMS` file;
  - `SHA256SUMS.minisig`, signed by a key held only by CI. The public key
    is compiled into `vessel`, which verifies it with the small pure-Rust
    `minisign-verify` crate.
- **Install steps**, all over the ADR 0007 `ssh` runner:
  1. Detect the platform: `uname -sm` sent to `sh -s`, last line only.
     Anything other than the four targets is refused with the raw output.
  2. If `vesseld-<v> version` already prints the expected version and
     target, stop.
  3. On the remote, `curl -fsSL --proto '=https'`, or else `wget`, into
     `bin/.vesseld-<v>.<random>.tmp`, then unpack.
  4. If the remote has neither tool, or cannot reach GitHub, the client
     downloads the archive with its local `curl`. Vessel builds no TLS
     stack (ADR 0005 amendment) and gains no HTTP client for this. The
     client then pipes the archive through `ssh` stdin into the temporary
     file. No `scp`/`sftp` dependency.
  5. The remote computes the hash (`sha256sum`, or `shasum -a 256` on
     macOS). The client compares it with its verified `SHA256SUMS` entry;
     on a mismatch it deletes the file and stops.
  6. `chmod 755` and `mv -f` onto the final name, in the same directory
     (atomic). Then run `vesseld-<v> version` once more.
  7. `vesseld-<v> cleanup` (below).
- **Development builds.** When the local `vessel` binary was built for the
  remote's target, the installer can upload the sibling `vesseld` (`--from
  local`), named by content hash. That is how M2 is exercised before the
  first release. A cross build is the developer's job.
  - "The remote's target" means its OS and architecture (coordinator,
    2026-11-06). A local `-linux-gnu` build is accepted for a Linux remote
    of the same architecture, always under its `-dev-<hash>` name; only a
    verified release archive gets the plain `vesseld-<version>` name.
  - Safeguard: a gnu build needs a glibc at least as new as the build
    machine's. The install script therefore runs the uploaded file's
    `version` on the remote *before* the rename. If it fails to start
    (non-zero exit, such as "GLIBC_2.xx not found") or reports another
    version, the file is deleted and the error suggests a musl build
    (`--target x86_64-unknown-linux-musl`) or a release. An unrunnable
    binary never has a final name.
- **Switching versions.** Installing never touches the running node.
  - A client of version B arriving at a node of version A connects through
    `vesseld-B attach`, a byte pump that does not care about the node's
    version, and then negotiates as usual.
  - If the ranges overlap, it simply works, and `vessel node status` shows
    that an update is available.
  - If they do not overlap, the client says so and points to `vessel node
    update`.
- **Updating with live sessions.** `vessel node update NAME` installs the
  new version, then:
  - with no live session: it stops the node (`node.stop{Now}`), and the
    next `attach` starts the new version;
  - with live sessions and no flag: it lists them (harness, mode,
    status) and stops, changing nothing;
  - with `--when-idle`: it sends `node.stop{WhenIdle}`. The node refuses
    new sessions with `unavailable`, reports `NodeState::Draining`, and
    exits after the last live session's final status is committed.
    Attached clients see the end and reconnect into the new version;
  - with `--now`: after a confirmation, `node.stop{Now}`. Sessions end
    `Interrupted`, as at any M1 shutdown, and the CLI prints the `--resume`
    command for each structured one.
- **Cleanup.** `vesseld cleanup` runs after every install and deletes
  `bin/vesseld-*` files except three: the file named in `vesseld.lock` (the
  running node, ADR 0007), the newest by modification time, and the binary
  running the cleanup.
  - A binary another `attach` still runs may be deleted; on Unix it keeps
    working until it exits.
  - Temporary files older than one hour are removed too.
- **The session host** (option 5) is the intended later answer for
  updating without interrupting. It is out of M2. When it comes, it reuses
  this layout: the host binary is versioned too, and is updated far less
  often.

## As built in milestone 2

Plan decision 1 replaces steps 3 and 4 for milestone 2: nothing is
downloaded on the remote.

- `vessel` gets the release archive and `SHA256SUMS` locally through the
  authenticated GitHub API, or uses a local build. The API is reached with
  the GitHub CLI (`gh release download v<version> --repo Team-WICKED/vessel
  --pattern <archive> --pattern SHA256SUMS`), using the login the user
  already has. `vessel` therefore holds no token and builds no TLS stack.
  The archive is checked against `SHA256SUMS` locally, and again on the
  remote after the upload. Without `gh`, the error points to `--from
  local`. It then streams the
  bytes into the remote's temporary file over `ssh` stdin.
- One fixed `sh -c` script does steps 5 and 6 on the remote, with the
  paths, the expected digest and the expected version as quoted
  arguments. In order, it:
  1. writes `bin/.vesseld-<name>.<random>.tmp`;
  2. checks its SHA-256 (`sha256sum`, else `shasum -a 256`);
  3. unpacks `vesseld` from a release archive with `tar -xzO`;
  4. runs `version` (the safeguard above);
  5. `mv -f`s the file onto the final name in the same directory.

  A trap removes the temporary files on every way out. The script's own
  exit codes are 90 (no SHA-256 tool), 91 (checksum mismatch), 92 (does
  not run) and 93 (another version).
- The script is one line with no `!` and no `\\`. It passes through the
  user's login shell first, which may be `csh` (no newlines inside quotes)
  or `fish` (reads `\\` inside single quotes).
- An existing binary whose `version` already reports the expected version
  is not uploaded again.
- Signing (`SHA256SUMS.minisig`) waits for the owner's decision on public
  releases. Until then the published SHA-256 is the integrity check.
- **Update and development builds** (coordinator, 2026-11-06). `vessel
  node update` leaves a node alone when it already runs this release:
  nothing new was uploaded, and the node reports the client's version.
  - A development build (`-dev-<hash>`) shares its version string with
    every other build of that version, so "already runs it" cannot be told
    from the version.
  - For those, `update` always goes through the restart policy above
    (stop at once without live sessions; otherwise refuse, `--when-idle`
    or `--now`).
  - Cost: an `update` with an unchanged dev build restarts the node
    anyway. The alternative, reporting the running executable over the
    protocol, is a wire change for a development-only case.
- **Release builds** (coordinator, 2026-11-06). Both Linux targets
  (`x86_64` and `aarch64`, static musl) are built on the x86_64 runner
  with `cargo-zigbuild`. Zig is both the cross linker and the C compiler
  for the bundled SQLite.
  - The plan preferred native arm64 runners where GitHub offers them.
    Hosted arm64 Linux runners are not available to every private
    repository, and zig needs none.
  - The aarch64 Linux binary cannot be run on its build runner, so the
    workflow checks its `version` line only on the targets that run there
    (x86_64 Linux and arm64 macOS). The remote install checks every
    target's `version` again before renaming (above).
  - Both macOS targets build on the arm64 macOS runner; Apple's toolchain
    cross-compiles to x86_64.

## Why

- Running the client's exact version removes a whole class of "which build
  is that node?" questions. The protocol range still lets an older node
  serve a newer client, so updating stays the user's choice, not a forced
  restart.
- Content-addressed dev builds keep side-by-side installs correct for
  builds that share a version number, which is every build before the first
  release.
- A signed checksum costs one small dependency and one CI secret, and
  closes the gap Zed leaves open. A binary that runs every harness on a
  machine (with the user's keys and repositories in reach) is exactly what
  an attacker would like to replace.
- Downloading on the remote first keeps large transfers off the user's
  uplink. Falling back to local `curl` plus `ssh` stdin covers air-gapped
  remotes and adds no dependency.
- The drain and refuse options use what M1 already guarantees (the shutdown
  order, `Interrupted`, ACP resume). They need only one RPC and one node
  state, instead of a new process architecture.

## Consequences

- **Protocol 3:** `node.stop { when: Now | WhenIdle }` and the additive
  `NodeInfo.state`. A client that negotiated 2 cannot drain an old node; it
  can only tell the user to stop it.
- **CI gains a release workflow and a signing secret.** Losing the key
  means rotating the public key compiled into `vessel`, which in turn means
  releasing a new client.
- **Private repository:** remote downloads need a token. The plan's open
  decision 1 picks between making releases public and always downloading
  locally with `gh`.
- **Terminal sessions still die on update** unless the user waits for them
  (`--when-idle`). Only the session host removes that, and this ADR records
  it as the next step rather than pretending drain is equivalent.
- **Windows remotes are not supported:** no target, no auto-start. The
  installer refuses them with the detected platform.
