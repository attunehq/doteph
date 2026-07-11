# Architecture

This document explains the key design decisions behind `eph` and how the
pieces fit together. For where each decision lives in the code, see
[Internals](internals.md).

## Shape of the program

`eph` is a small Rust CLI built as a thin binary over a reusable library:

- `src/main.rs`: the `clap` front end. Defines the CLI, sets up logging, and
  dispatches each subcommand to a small `cmd_*` glue function. Nothing here is
  public API.
- `src/lib.rs`: the library crate (`eph`) that holds all reusable logic, split
  into modules: `parser`, `workspace`, `service`, `env`, `skills`, `update`
  (the self-updater that pulls, verifies, and swaps in a GitHub release),
  `prune`, and the crate-internal `proc` (the cross-platform shell and
  process-control layer). The file watcher behind `eph dev --watch`
  (`src/watch.rs`) is a binary-side module, not part of the library.
- `src/skills.rs` and `skills/`: the agent skills bundled into the binary.
  Each `skills/<slug>/SKILL.md` is embedded with `include_str!`. `eph skills
  install` writes it into a consuming repo's `.claude/skills/` and
  `.agents/skills/`; `eph skills check` fails closed when a checked-in copy
  has drifted from the embedded source (a deterministic, version-independent
  lint wired into `.github/workflows/ci.yml`); `eph skills list` shows what is
  bundled. These teach an agent to *use* `eph`. They are distinct from the
  repo-local Rust skills under `.agents/skills/`, which guide agents working
  *on* `eph`.

Keeping the logic in a library means behavior is unit- and doc-testable
without going through the CLI, and the binary stays a dumb adapter.

External dependencies of note: `clap` (CLI), `bollard` (async Docker API),
`tokio` (runtime), `serde`/`serde_json` (state and JSON output), `sha2`/`hex`
(workspace IDs), `dirs` (platform data directory), `shell-words` (command
parsing), `sysinfo` (process-table liveness and the Windows descendant-tree
teardown walk), `libc` (Unix only: signaling a `run=` shell's process group
via `killpg`), `fd-lock` (the per-workspace and prune advisory file locks; see
[Internals](internals.md#service)), and `tracing` (logging to stderr).
`eph update` adds `ureq` (rustls HTTPS, no system TLS), `flate2`/`tar`
(pure-Rust archive extraction), `self-replace` (the platform-correct in-place
binary swap), and `semver` (version comparison).
`dunce` supplies Docker-compatible canonical Windows paths. `indexmap`
preserves `.eph` declaration order. `notify` and `globset` implement
`eph dev --watch`; `socket2` configures the preview gate for reliable rebinding.

## Core concepts

### Workspace isolation

Each directory containing a `.eph` file is a workspace, identified by the
SHA-256 of its canonicalized absolute path. The first 16 hex characters (the
short ID) namespace everything `eph` creates:

```
/Users/grace/projects/myapp   ->  eph-a1b2c3d4e5f60718-postgres
/Users/grace/projects/myapp2  ->  eph-e5f6a7b8c9d00112-postgres
```

This keeps eph-managed container, image, volume, and Compose project names
separate across checkouts. Direct container ports are assigned by Docker, and
`run=` services can request `port=auto`. Fixed host-process ports and bindings
declared inside a Compose file remain the configuration author's responsibility.
Canonicalizing the path makes the ID stable across symlinks and relative
addressing.

If the state root contains an 8-character namespace whose workspace metadata
matches the full workspace ID, eph continues using that namespace until
`eph clean`. An 8-character state directory that cannot be verified blocks
workspace construction so eph cannot create a second namespace beside unknown
resources.

### Auto port assignment

Container ports are published on random host ports chosen by Docker, bound to
`127.0.0.1` only. This eliminates port conflicts entirely and keeps services
off the local network. Assigned ports are recorded in state and surfaced
through interpolation, so configuration never hardcodes a host port.

`run=` (non-container) services are the exception: the process binds its own
port, so `eph` reports a numeric declared port as-is. With `port=auto`, eph
allocates a free host port itself, injects it through the process
environment. An auto-port process that exits during startup is relaunched on a
fresh port, for up to four attempts. Fixed-port and portless processes fail on
an early exit.

### Service state

Running-service information (backend handles, assigned ports, process PIDs) is
persisted to the platform local-data directory under
`eph/<short_id>/state.json`. Workspace metadata lives beside it in
`eph/<short_id>/workspace.json`; system prune uses that file to decide whether
the recorded workspace path still exists.

- Linux: `~/.local/share/eph/<short_id>/state.json`
- macOS: `~/Library/Application Support/eph/<short_id>/state.json`
- Windows: `%LOCALAPPDATA%\eph\<short_id>\state.json`

`EPH_STATE_ROOT` overrides the parent directory (the `eph` above `<short_id>`)
in place of the platform default; `workspace::state_root()` checks it first and
rejects relative paths.

State lets `eph status` and `eph env` work without re-deriving everything,
lets assigned ports survive terminal restarts, and records which resources
belong to a workspace. It also retains a canonical runtime fingerprint and the
backend type that created each service. This lets `up` detect effective config
drift, including source-type and resolved dependency-port changes, and tear down
the old resource through recorded backend truth before creating the new one.
Writes are atomic (a temp file, renamed over the real
one) so a crash mid-write cannot leave a truncated `state.json`; a file that
still fails to parse is quarantined to `state.json.corrupt` rather than treated
as fatal, and eph
continues with fresh empty state. Every command that mutates state (`up`,
`down`, `clean`) holds an OS advisory lock scoped to the workspace (a file
next to the state directory, released automatically if the process dies) for
the duration of the operation, so two overlapping invocations in the same
workspace serialize instead of racing each other's writes.

`eph clean` deletes this directory for the current workspace. `eph system
prune` scans every state directory and removes Docker resources by the
`eph-<short_id>-` namespace when the recorded workspace path is missing or
empty, but only once it has confirmed the workspace is actually dead: a
recorded path that no longer resolves could mean the workspace was moved or
renamed rather than deleted, so prune first checks for any running container
or live `run=` process under that namespace and skips (reports, does not
remove) a workspace that still has either, unless `--force-live` is passed.
For `run=` services every lifecycle command signals only PIDs whose current
process identity matches the identity saved at launch. Startup stops the child
and fails if that identity cannot be captured. Process entries without an
identity are left alone; workspace-local teardown reports how to stop them manually, and
system prune reports a warning. A real (non-dry-run) prune also asks for
confirmation before removing anything, unless `--yes` is passed or there is
nothing to remove;
off a non-interactive terminal without `--yes` it refuses rather than
guessing.

## File format

The `.eph` format is designed to be:

1. **Familiar**: it looks like the `.env` and INI files developers already
   know. A valid `.env` file is a valid `.eph` file.
2. **Minimal**: simple cases need little syntax.
3. **Flat**: no deep nesting or significant indentation.

### Why not YAML/TOML/JSON?

| Format | Issue |
|--------|-------|
| YAML | Indentation errors, type-coercion surprises |
| TOML | Verbose for this use case, requires quotes |
| JSON | No comments, not human-friendly |
| HCL | Learning curve, overkill |

The trade-off is a small custom parser (see [Internals](internals.md#parser))
and one sharp edge documented for users: comments must be on their own line,
since there is no inline-comment stripping. The parser favors hard errors over
guessing: an unknown key inside a section, a duplicated section or property, an
invalid service or variable name, and a malformed interpolation are all
rejected at parse time rather than silently reinterpreted or overwritten. A
top-level variable placed directly after a service section is the sharpest
case of this: sections do not end at blank lines, so it is rejected rather
than treated as ending the section, and the error names the two places a
variable can legally go (above the first section, or inside a reserved
`[env]` section).

## Service types

A service declares a source. The "no source" state is rejected at parse time,
so by the time a `Service` value exists it always names a real way to start.
Multiple source keys are also rejected, so exactly one source is an enforced
boundary invariant.

- **Docker image** (most common): pull if needed, create a workspace-named
  container, map ports with auto-assignment, create per-workspace named
  volumes, poll the health check.
- **Dockerfile**: shell out to `docker build`, tag the image
  `eph-<short_id>-<service>`, then run it like an image service.
- **Docker Compose**: shell out to
  `docker compose -p eph-<short_id>-<service> up -d`; query
  `docker compose port` for mapped ports; tear down with `compose down`.
- **Shell command** (`run=`): spawn a background process via the platform
  shell, track its PID, run health checks on the host.

The image and Dockerfile paths use the `bollard` Docker API directly; the
Compose and Dockerfile-build paths shell out to the `docker` CLI because those
operations are awkward to reproduce over the API.

### Cross-platform process control

The host-side bits of the `run=` path (the shell, PID liveness, and teardown)
are platform-abstracted in [`src/proc.rs`](../../src/proc.rs): the shell is
`sh -c` on Unix and `cmd /C` on Windows. On Unix the kill path sends `SIGTERM`
then `SIGKILL`; on Windows, which has no POSIX signals, both the graceful and
forced stops become a hard terminate.

Teardown kills the whole process tree rooted at the tracked PID, not just that
one process. The tracked PID is the shell wrapper, and the real service is its
child, so killing only the wrapper would orphan the service (or, for a
compound command or a pipeline, its children). eph is daemonless (a separate
`eph down` reads the PID from state and tears the service down), so the
mechanism must be addressable across `eph` invocations by the PID alone, and
that constraint splits the two platforms:

- On Unix the shell is spawned as the leader of a new process group
  (PGID == PID, via `process_group(0)`), and teardown signals the group with
  `killpg`. This is **race-free**: every descendant is in the group, including
  one forked after any snapshot would have been taken. `is_alive` likewise
  probes the group (`killpg(pid, 0)`), so a service whose shell exited but
  left a backgrounded child still reads as up.
- On Windows there is no group an unrelated `eph down` can address. A Job
  Object is the natural fit, but a named job cannot be reattached after the
  `eph up` that created it exits: closing the last handle releases the
  object's name immediately (even while its processes run), so a later
  `OpenJobObject` by name fails, and keeping the name alive would need a
  persistent handle holder a daemonless CLI lacks. Teardown therefore walks
  parent links in a `sysinfo` snapshot from the tracked PID and terminates the
  whole descendant tree. A child spawned after that snapshot can escape; that
  is the accepted limit of snapshot-based teardown.

The same `sysinfo` descendant walk is the Unix fallback when the recorded shell
does not lead its own process group.

### Reconciling compose services

`ServiceManager::status` reconciles state by looking up a container named
`eph-<short_id>-<service>`, which exists for image and dockerfile services
(and is approximated by an identity-checked PID for `run` services). Compose
names its own containers (`<project>-<service>-N`), so a compose service has
no such container. It is instead recorded with a `compose:<project>` id and
detected by `DockerClient::compose_project_running`, which lists containers
carrying the `com.docker.compose.project=<project>` label. This is what lets
compose services appear in `status` and resolve their `expose` ports in
`eph env`. A service's `env.X` values are resolved (`${service.property}`
interpolation) and passed as `docker compose`'s process environment for `up`
and port discovery, the same way they are resolved into an
image/dockerfile container's own environment, so a compose file's `${VAR}`
substitution sees the same connection details eph itself resolves. Teardown
uses only the recorded project name, so it neither rereads a changed file nor
requires its substitution environment. It remains coarser than for direct
containers: `stop_service` runs
`docker compose down` regardless of the `--rm` flag, but only when the
project is known to be up (from `status` or recorded state); a service never
brought up is a no-op, matching the container path, and a `docker compose
down` that does run and fails is a real error that aborts teardown rather
than being swallowed. `clean` removes named volumes declared by image and
Dockerfile services. Compose services cannot declare `.eph` volumes, and
Compose-internal volumes remain owned by `docker compose`.

### Runtime reconciliation

`ServiceManager` prepares a canonical runtime specification before deciding
whether an existing resource can be reused. Stable serialization of sorted
environment maps feeds a SHA-256 fingerprint covering source, immutable image
ID, declared ports, fully resolved environment, volumes, health settings, build
context, and command argv. `run=` fingerprints include the final top-level,
metadata, service environment, and assigned self port, so a dependency port
change restarts consumers.

Dockerfile sources build on every `up` through Docker's cache and fingerprint
the resulting image ID. A matching live or stopped resource can be reused, but
its declared health check is rerun. A mismatch, a state record without a
fingerprint, or a source-type change discards the recorded backend before creation. Failed starts
are discarded and persisted immediately, preventing a later `up` from adopting
an unhealthy container, Compose project, or process.

## Health checks

`eph up` waits for every service to be healthy before running any
`post-start` hooks and returning. The mechanism differs by service type,
deliberately:

- **image/dockerfile**: the command runs inside the container via
  `docker exec`, split on whitespace, with **no shell**. This avoids depending
  on a shell being present in the image, at the cost of not supporting pipes,
  redirects, or expansion. Default timeout 30s, polled every 1s.
- **run/compose**: the command runs on the host through the platform shell
  (`sh -c` on Unix, `cmd /C` on Windows), so full shell syntax is available in
  that platform's dialect. Default timeout 30s (run) or 60s (compose).

With no health check, `eph` waits a fixed 500 ms and proceeds.

## Lifecycle hooks

Four hooks bracket a service, all run on the host via the platform shell in
the workspace directory:

- `pre-start` runs just before a service is created, in phase 1 of `eph up`,
  so prep the service depends on (codegen, a generated config) finishes
  first. It sees the services already up at that point, but not its own
  not-yet-assigned port. A failing hook aborts `up` before the service
  starts.
- `post-start` runs in a **second phase** of `eph up`: every targeted service
  is first brought to a healthy state, then every service's `post-start`
  hooks run. Deferring hooks this way lets a hook reference any other
  service's resolved port. `pre-start` and `post-start` both run on **every**
  `eph up` regardless of source type or start path (fresh create, restart,
  reuse), so they are expected to be idempotent. A failing `post-start`
  aborts `up`.
- `pre-stop` runs before a service stops. A failure is propagated and aborts
  `eph down` / `eph clean`, leaving the service running so the hook can be
  fixed and retried.
- `post-stop` runs after a service has stopped, for cleanup eph cannot do
  itself. It sees the same pre-teardown snapshot as `pre-stop`. A failure is
  propagated and aborts the rest of teardown; because the service is already
  stopped, a later `down` will not re-run it.

All four receive eph's resolved environment (the `eph env` variables, `EPH_*`
metadata, and the service's own `env.X`); `eph run` exposes the same
environment to arbitrary commands. Every execution boundary uses tracked,
strict interpolation: an unresolved runtime reference is an error before a
child starts. `eph env` renders explicit unsets and a shell failure for affected
variables, while JSON omits them; both forms exit non-zero.

## Teardown levels

Three deliberately distinct levels:

- `eph down`: stop services, leave containers and volumes in place for a fast
  restart. Removes each service's own state entry as it is actually stopped
  (not a wholesale clear), and does the same for any state entry whose
  section was later renamed or deleted from the `.eph` file, so a rename never
  strands a running container beyond `down`'s reach.
- `eph down --rm`: additionally remove the stopped containers. Named-volume
  data is preserved; the next `up` recreates containers.
- `eph clean`: full reset. Remove containers (and Compose projects and
  processes), including ones recorded under a renamed or deleted section;
  remove per-workspace **named volumes** (data loss); sweep Docker itself for
  any leftover container or volume still carrying the workspace's
  `eph-<short_id>-` prefix (a service renamed before state recorded it, or a
  resource orphaned by a crash before state was written); and delete the
  state directory. Bind mounts are never removed. Reports how many services,
  volumes, and the state directory were actually removed, not how many the
  `.eph` file declares.

## CLI design

Commands follow a simple, predictable shape:

```
eph up [services...]            # start
eph down [--rm] [services...]   # stop (optionally remove containers)
eph clean                       # full reset
eph system prune                # remove resources for deleted workspaces
eph dev [service]               # foreground the stack for a preview server
eph status                      # show state
eph env [-f format]             # export resolved environment
eph run <cmd>...                # run a command with the resolved environment
eph logs [service]              # service logs across all source types
eph check                       # validate the .eph file
eph info                        # workspace metadata
eph skills <install|check|list> # manage the bundled agent skills
eph update [--check] [--force]  # self-update to the latest release
```

`eph env` writes shell-ready output to stdout while all logs go to stderr. A
fully resolved result composes with `eval "$(eph env)"`; an unresolved result
clears affected variables and fails, preventing stale shell state.
