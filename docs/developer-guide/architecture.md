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
  into four modules: `parser`, `workspace`, `service`, and `env`.

Keeping the logic in a library means the behavior is unit- and doc-testable
without going through the CLI, and the binary stays a dumb adapter.

External dependencies of note: `clap` (CLI), `bollard` (async Docker API),
`tokio` (runtime), `serde`/`serde_json` (state and JSON output), `sha2`/`hex`
(workspace IDs), `dirs` (platform data directory), `shell-words` (command
parsing), and `tracing` (logging to stderr).

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
`eph/<short_id>/state.json`:

- Linux: `~/.local/share/eph/<short_id>/state.json`
- macOS: `~/Library/Application Support/eph/<short_id>/state.json`
- Windows: `%LOCALAPPDATA%\eph\<short_id>\state.json`

State lets `eph status` and `eph env` work without re-deriving everything, lets
assigned ports survive terminal restarts, and records which resources belong to a
workspace. `eph clean` deletes this directory.

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
- **Shell command** (`run=`) - spawn a background process via `sh -c`, track its
  PID, run health checks on the host.

The image and Dockerfile paths use the `bollard` Docker API directly; the
Compose and Dockerfile-build paths shell out to the `docker` CLI because those
operations are awkward to reproduce over the API.

> **Known limitation: Compose tracking.** `ServiceManager::status` reconciles
> state by looking up a single container named `eph-<short_id>-<service>`, which
> exists for image/dockerfile services and is approximated by a tracked PID for
> `run` services. Compose services have no such container (Compose names its own
> containers), so they are recorded in `state.json` but never reappear in
> `status` - and therefore never in `eph env` interpolation. As a result, a
> compose service's `expose` ports do not resolve after `up`. Teardown is also
> coarser: `stop_service` always runs `docker compose down` regardless of the
> `--rm` flag. Closing this gap (resolving `compose:<project>` entries and saved
> ports in `status`) is a worthwhile future change; until then the user docs
> describe compose support as deliberately thin.

## Health checks

`eph up` waits for a service to be healthy before running `post-start` hooks and
returning. The mechanism differs by service type, deliberately:

- **image/dockerfile**: the command runs inside the container via `docker exec`,
  split on whitespace - **no shell**. This avoids depending on a shell being
  present in the image, at the cost of not supporting pipes/redirects/expansion.
  Default timeout 30s, polled every 1s.
- **run/compose**: the command runs on the host through `sh -c`, so full shell
  syntax is available. Default timeout 30s (run) / 60s (compose).

With no health check, `eph` waits a fixed 500 ms and proceeds.

## Lifecycle hooks

- `post-start` runs after a service is healthy, on the host via `sh -c`, in the
  workspace directory. For `image`/`dockerfile` services it runs only on **fresh
  container creation**, not when a stopped container is restarted (so migrations
  are not re-run on every `up`); `run` services re-run it whenever the tracked
  PID is not alive, and `compose` services re-run it on every `up` (they are
  excluded from the existing-container guard). A failing hook aborts `up`.
- `pre-stop` runs before a service stops; failures are logged, not propagated, so
  a stale service does not block teardown.

## Teardown levels

Three deliberately distinct levels:

- `eph down` - stop services, leave containers and volumes in place for a fast
  restart. Clears in-memory/persisted state entries.
- `eph down --rm` - additionally remove the stopped containers. Named-volume data
  is preserved; the next `up` recreates containers (and re-runs `post-start`).
- `eph clean` - full reset: remove containers (and Compose projects / processes),
  remove per-workspace **named volumes** (data loss), and delete the state
  directory. Bind mounts are never removed.

## CLI design

Commands follow a simple, predictable shape:

```
eph up [services...]            # start
eph down [--rm] [services...]   # stop (optionally remove containers)
eph clean                       # full reset
eph status                      # show state
eph env [-f format]             # export resolved environment
eph check                       # validate the .eph file
eph info                        # workspace metadata
```

`eph env` writes shell-ready output to stdout while all logs go to stderr, so it
composes cleanly with `eval "$(eph env)"` and pipes. This integrates with
existing shell workflows without requiring shell hooks or special integration.
