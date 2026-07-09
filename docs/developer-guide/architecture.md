# Architecture

This document explains the key design decisions behind `eph` and how the pieces
fit together. For where each decision lives in the code, see
[Internals](internals.md).

## Shape of the program

`eph` is a small Rust CLI built as a thin binary over a reusable library:

- `src/main.rs` - the `clap` front end. Defines the CLI, sets up logging, and
  dispatches each subcommand to a small `cmd_*` glue function. Nothing here is
  public API.
- `src/lib.rs` - the library crate (`eph`) that holds all reusable logic, split
  into modules: `parser`, `workspace`, `service`, `env`, `skills`, `update` (the
  `eph update` self-updater that pulls, verifies, and swaps in a GitHub release),
  and the crate-internal `proc` (the cross-platform shell + PID-control layer).
- `src/skills.rs` / `skills/` - the agent skills bundled into the binary. Each
  `skills/<slug>/SKILL.md` is embedded with `include_str!`; `eph skills install`
  writes it into a consuming repo's `.claude/skills/` and `.agents/skills/`, `eph
  skills check` fails closed when a checked-in copy has drifted from the embedded
  source (a deterministic, version-independent lint wired into
  `.github/workflows/ci.yml`), and `eph skills list` shows what is bundled. These
  teach an agent to *use* `eph`, and are distinct from the repo-local Rust skills
  under `.agents/skills/` that guide agents working *on* `eph`.

Keeping the logic in a library means the behavior is unit- and doc-testable
without going through the CLI, and the binary stays a dumb adapter.

External dependencies of note: `clap` (CLI), `bollard` (async Docker API),
`tokio` (runtime), `serde`/`serde_json` (state and JSON output), `sha2`/`hex`
(workspace IDs), `dirs` (platform data directory), `shell-words` (command
parsing), `sysinfo` (process-table liveness and the Windows descendant-tree
teardown walk), `libc` (Unix-only: signaling a `run=` shell's process group via
`killpg`), and `tracing` (logging to stderr). `eph update` adds `ureq` (rustls
HTTPS, no system TLS), `flate2`/`tar` (pure-Rust archive extraction),
`self-replace` (the platform-correct in-place binary swap), and `semver` (version
comparison).

## Core concepts

### Workspace isolation

Each directory containing a `.eph` file is a workspace, identified by the
SHA-256 of its canonicalized absolute path. The first 8 hex characters (the
short ID) namespace everything `eph` creates:

```
/Users/alice/projects/myapp   ->  eph-a1b2c3d4-postgres
/Users/alice/projects/myapp2  ->  eph-e5f6g7h8-postgres
```

This guarantees that two checkouts of the same repo, or two developers on one
machine, never collide on container names, volume names, or ports. Canonicalizing
the path first makes the ID stable across symlinks and relative addressing.

### Auto port assignment

Container ports are published on random host ports chosen by Docker, bound to
`127.0.0.1` only. This eliminates port conflicts entirely and keeps services off
the local network. The assigned ports are recorded in state and surfaced through
interpolation, so configuration never hardcodes a host port.

`run=` (non-container) services are the exception: the process binds its own
port, so `eph` reports the declared port as-is rather than remapping it.

### Service state

Running-service information (container IDs, assigned ports, process PIDs) is
persisted to the platform local-data directory under
`eph/<short_id>/state.json`. Workspace metadata lives beside it in
`eph/<short_id>/workspace.json`; system prune uses that file to decide whether the
recorded workspace path still exists.

- Linux: `~/.local/share/eph/<short_id>/state.json`
- macOS: `~/Library/Application Support/eph/<short_id>/state.json`
- Windows: `%LOCALAPPDATA%\eph\<short_id>\state.json`

State lets `eph status` and `eph env` work without re-deriving everything, lets
assigned ports survive terminal restarts, and records which resources belong to a
workspace. `eph clean` deletes this directory for the current workspace. `eph
system prune` scans every state directory and removes Docker resources by the
`eph-<short_id>-` namespace when the recorded workspace path is missing or empty.
For `run=` services it signals only PIDs whose current process identity matches
the identity saved at launch; legacy process entries are warned about and left
alone.

## File format

The `.eph` format was designed to be:

1. **Familiar** - it looks like the `.env` + INI files developers already know. A
   valid `.env` file is a valid `.eph` file.
2. **Minimal** - simple cases need little syntax.
3. **Flat** - no deep nesting or significant indentation.

### Why not YAML/TOML/JSON?

| Format | Issue |
|--------|-------|
| YAML | Indentation errors, type-coercion surprises |
| TOML | Verbose for this use case, requires quotes |
| JSON | No comments, not human-friendly |
| HCL | Learning curve, overkill |

The trade-off is a small custom parser (see [Internals](internals.md#parser)) and
a couple of sharp edges documented for users: comments must be on their own line,
and an unknown `SCREAMING_SNAKE_CASE` key inside a section is reclassified as a
trailing top-level variable (with a warning) rather than an error.

## Service types

A service declares a source. The "no source" state is rejected at parse time, so
by the time a `Service` value exists it always names a real way to start.
(Multiple source keys are not validated - the parser simply keeps the last one,
so "exactly one" is a convention, not an enforced rule.)

- **Docker image** (most common) - pull if needed, create a workspace-named
  container, map ports with auto-assignment, create per-workspace named volumes,
  poll the health check.
- **Dockerfile** - shell out to `docker build`, tag the image
  `eph-<short_id>-<service>`, then run it like an image service.
- **Docker Compose** - shell out to `docker compose -p eph-<short_id>-<service>
  up -d`; query `docker compose port` for mapped ports; tear down with
  `compose down`.
- **Shell command** (`run=`) - spawn a background process via the platform shell,
  track its PID, run health checks on the host.

The image and Dockerfile paths use the `bollard` Docker API directly; the
Compose and Dockerfile-build paths shell out to the `docker` CLI because those
operations are awkward to reproduce over the API.

The host-side bits of the `run=` path (the shell, plus PID liveness and
teardown) are platform-abstracted in [`src/proc.rs`](../../src/proc.rs): the
shell is `sh -c` on Unix and `cmd /C` on Windows. On Unix the kill path sends
`SIGTERM` then `SIGKILL`; on Windows, which has no POSIX signals, both the
graceful and forced stops become a hard terminate (`sysinfo` uses the built-in
`taskkill /F`, so no extra setup or WSL is needed).

Teardown kills the whole process tree rooted at the tracked PID, not just that
one process. The tracked PID is the shell wrapper (`cmd /C` on Windows, `sh -c`
on Unix), and the real service is its child, so killing only the wrapper would
orphan the service (or, for a compound command or a pipeline, its children). eph
is daemonless (a separate `eph down` reads the PID from state and tears the
service down), so the mechanism has to be addressable across `eph` invocations by
the PID alone, and that constraint splits the two platforms:

- On Unix the shell is spawned as the leader of a new process group (PGID == PID,
  via `process_group(0)`), and teardown signals the group with `killpg`. This is
  **race-free**: every descendant is in the group, including one forked after any
  snapshot would have been taken. `is_alive` likewise probes the group
  (`killpg(pid, 0)`), so a service whose shell exited but left a backgrounded
  child still reads as up.
- On Windows there is no group an unrelated `eph down` can address. A Job Object
  is the natural fit, but a named job cannot be reattached after the `eph up` that
  created it exits: closing the last handle releases the object's name
  immediately (even while its processes run), so a later `OpenJobObject` by name
  fails, and keeping the name alive would need a persistent handle holder a
  daemonless CLI lacks. Teardown therefore walks parent links in a `sysinfo`
  snapshot from the tracked PID and terminates the whole descendant tree. A child
  spawned after that snapshot can escape, the accepted limit of snapshot-based
  teardown.

The same `sysinfo` descendant walk is the Unix fallback for a service recorded
before eph grouped its shells (legacy on-disk state, where the wrapper leads no
group).

> **Reconciling compose services.** `ServiceManager::status` reconciles state by
> looking up a container named `eph-<short_id>-<service>`, which exists for
> image/dockerfile services (and is approximated by a tracked PID for `run`
> services). Compose names its own containers (`<project>-<service>-N`), so a
> compose service has no such container. It is instead recorded with a
> `compose:<project>` id and detected by `DockerClient::compose_project_running`,
> which lists containers carrying the `com.docker.compose.project=<project>`
> label. This is what lets compose services appear in `status` and resolve their
> `expose` ports in `eph env`. Teardown remains coarser than for direct
> containers: `stop_service` always runs `docker compose down` regardless of the
> `--rm` flag, and `clean` removes only the named volumes declared in the `.eph`
> file (Compose-internal volumes are left to `docker compose`).

## Health checks

`eph up` waits for every service to be healthy before running any `post-start`
hooks and returning. The mechanism differs by service type, deliberately:

- **image/dockerfile**: the command runs inside the container via `docker exec`,
  split on whitespace - **no shell**. This avoids depending on a shell being
  present in the image, at the cost of not supporting pipes/redirects/expansion.
  Default timeout 30s, polled every 1s.
- **run/compose**: the command runs on the host through the platform shell
  (`sh -c` on Unix, `cmd /C` on Windows), so full shell syntax is available in
  that platform's dialect. Default timeout 30s (run) / 60s (compose).

With no health check, `eph` waits a fixed 500 ms and proceeds.

## Lifecycle hooks

Four hooks bracket a service, all run on the host via the platform shell (`sh -c`
/ `cmd /C`) in the workspace directory:

- `pre-start` runs just before a service is created, in phase 1 of `eph up`, so
  prep the service depends on (codegen, a generated config) finishes first. It
  sees the services already up at that point (backing services start before `run=`
  apps) but not its own not-yet-assigned port. A failing hook aborts `up` before
  the service starts.
- `post-start` runs in a **second phase** of `eph up`: every targeted service is
  first brought to a healthy state, then every service's `post-start` hooks run.
  Deferring hooks this way lets a hook reference any other service's resolved
  port. `pre-start` and `post-start` both run on **every** `eph up` regardless of
  source type or start path (fresh create, restart, reused), so they are expected
  to be idempotent. A failing `post-start` aborts `up`.
- `pre-stop` runs before a service stops; a failing hook is propagated and aborts
  `eph down` / `eph clean`, leaving the service running so the hook can be fixed
  and retried (the process/Compose teardown that follows is still best-effort).
- `post-stop` runs after a service has stopped, for cleanup eph cannot do itself.
  It sees the same pre-teardown snapshot as `pre-stop`. A failing hook is
  propagated and aborts the rest of teardown; because the service is already
  stopped, a later `down` will not re-run it.
- All four hooks receive eph's resolved environment (the `eph env` variables,
  `EPH_*` metadata, and the service's own `env.X`); the same environment is
  exposed to arbitrary commands by `eph run`.

## Teardown levels

Three deliberately distinct levels:

- `eph down` - stop services, leave containers and volumes in place for a fast
  restart. Clears in-memory/persisted state entries.
- `eph down --rm` - additionally remove the stopped containers. Named-volume data
  is preserved; the next `up` recreates containers. (`post-start` runs on every
  `up` regardless; `--rm` only changes whether the container is reused or rebuilt.)
- `eph clean` - full reset: remove containers (and Compose projects / processes),
  remove per-workspace **named volumes** (data loss), and delete the state
  directory. Bind mounts are never removed.

## CLI design

Commands follow a simple, predictable shape:

```
eph up [services...]            # start
eph down [--rm] [services...]   # stop (optionally remove containers)
eph clean                       # full reset
eph system prune                # remove resources for deleted/empty workspaces
eph status                      # show state
eph env [-f format]             # export resolved environment
eph run <cmd>...                # run a command with the resolved environment
eph check                       # validate the .eph file
eph info                        # workspace metadata
eph skills <install|check|list> # manage the bundled agent skills
eph update [--check] [--force]  # replace the running binary with the latest release
```

`eph env` writes shell-ready output to stdout while all logs go to stderr, so it
composes cleanly with `eval "$(eph env)"` and pipes. This integrates with
existing shell workflows without requiring shell hooks or special integration.
