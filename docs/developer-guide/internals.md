# Internals

A module-by-module map of the source, so you can find where a change belongs.
For the rationale behind these structures, see [Architecture](architecture.md).

## Module layout

```
src/
  main.rs        CLI front end (clap) + cmd_* dispatch glue. Not public API.
  watch.rs       Binary-side file watcher behind `eph dev --watch`.
  lib.rs         Library crate root; re-exports the public API.
  parser.rs      .eph -> AST (services, roles), plus interpolation.
  workspace.rs   Workspace resolution, IDs, naming, paths.
  service.rs     Docker client wrapper + ServiceManager + persisted state.
  proc.rs        Crate-internal platform shell + process control (sh/cmd).
  prune.rs       Cross-workspace stale-state discovery and resource removal.
  env.rs         Rendering resolved env vars for shell eval.
  skills.rs      Bundled agent skills: install / check / list.
  update.rs      Self-updater + the passive out-of-date nag.
tests/
  common/mod.rs  Shared test helpers.
  integration.rs Happy-path lifecycle tests against a live Docker daemon.
  stress.rs      Heavyweight, concurrent, multi-service suite (#[ignore]'d).
```

The public API is whatever `src/lib.rs` re-exports:

```rust
pub use env::{escape_bash, escape_fish, render, render_export, render_fish, render_json};
pub use parser::{EphFile, Service, ServiceSource, parse, resolve_interpolations};
pub use prune::{PruneOptions, PruneReport, prune};
pub use service::{LogOptions, RunningService, ServiceManager, resolve_env_vars};
pub use workspace::Workspace;
```

## parser

`src/parser.rs` turns `.eph` text into an `EphFile`.

**AST types:**

- `EphFile { env_vars: Vec<EnvVar>, services: IndexMap<String, Service>,
  roles_order: Option<RolesOrder> }`. The `IndexMap` preserves declaration
  order, which legacy-mode start order depends on.
- `EnvVar { name, value }`: the value is stored verbatim, including `${...}`
  placeholders (resolved later, not here).
- `Service { name, role, source, ports, env, volumes, pre_start, post_start,
  pre_stop, post_stop, healthcheck, ready_timeout_secs, build_context,
  command_override }`.
- `ServiceSource`: `Image | Dockerfile | Compose | Command`. There is
  intentionally no "unset" variant: see below.
- `PortMapping { name: Option<String>, container_port: u16, auto: bool }`.
  `auto` marks a `port=auto` declaration on a `run=` service.
- `RolesOrder { deps: IndexMap<String, Vec<String>> }`: the role dependency
  graph, from either the linear `roles_order=a,b` form or the DAG
  `[roles_order]` section. Roles-mode validation (every service has a listed
  role, every role is backed, edges known, graph acyclic) happens at parse
  time so a broken graph never reaches `eph up`.

**`parse(input) -> Result<EphFile>`** is a single line-oriented pass:

- Blank lines and `#`-leading lines are skipped. There is no inline-comment
  stripping, which is why a `#` after a value becomes part of the value.
- `[name]` opens a section; an empty `[]` is an error. `[roles_order]` is the
  one reserved section name and parses as role edges rather than a service.
  Sections are kept in a `Vec<ServiceBuilder>` with a name-to-index map, so
  re-opening a section name appends to the same builder and finalization
  order stays deterministic.
- `key=value` splits on the first `=`; both sides are trimmed; a single
  matching pair of surrounding quotes is stripped (`strip_quotes`).
- Inside a section, `parse_service_property` interprets known keys (`image`,
  `dockerfile`, `compose`, `run`, `role`, `command`, `port`, `port.<name>`,
  `env.<KEY>`, `volume`, `pre-start`, `post-start`, `pre-stop`, `post-stop`,
  `healthcheck`, `ready-timeout`, `context`, `expose.<name>`). Numeric values
  (`port`, `ready-timeout`, named ports, expose) error on bad input.
- An **unknown** key inside a section: if it looks like an env var name
  (`is_env_var_name`: non-empty SCREAMING_SNAKE_CASE), the section is ended
  and the key becomes a top-level `EnvVar` (with a `tracing::warn!`);
  otherwise it is a hard error. This is the deliberate "trailing env vars
  after sections" affordance that also swallows miscased property typos.
- After the pass, each `ServiceBuilder` is finalized with
  `ServiceBuilder::finish`, which **requires a source**. This is where the
  "service with no source" state is rejected, keeping it out of the returned
  `EphFile` entirely.

**`resolve_interpolations(input, resolver) -> String`** scans for `${...}`,
splits the content on the first `.` into `(service, property)`, and
substitutes `resolver(service, property)`. Anything the resolver declines
(`None`), or a placeholder with no `.`, is left verbatim. The resolver is
supplied by the caller (`cmd_env` in `main.rs`) and reads from running
services.

## workspace

`src/workspace.rs` is pure path and ID logic, with no I/O beyond
canonicalization and the data-dir lookup.

- `Workspace { path, id, short_id }`.
- `from_path` canonicalizes a directory and computes `id = sha256(path)`,
  `short_id = id[..8]`.
- `find_from_path` / `find_from_cwd` walk up from a start directory to the
  first ancestor containing a `.eph` file: the workspace resolution used by
  every command.
- Naming helpers: `container_prefix()` (`eph-<short_id>`),
  `container_name(svc)`, `volume_name(svc, vol)`, `eph_file_path()`.
- `state_dir()` returns `<dirs::data_local_dir()>/eph/<short_id>`.
- `save_metadata()` writes `<state_dir>/workspace.json`, which records the
  canonical workspace path for cross-workspace pruning.

## service

`src/service.rs` is the largest module: the Docker integration and the
lifecycle engine.

**`DockerClient`** wraps a `bollard::Docker`:

- `connect()` connects with local defaults and pings (this is the "is docker
  running?" check).
- `get_container(name)` filters by name and matches exactly (Docker's name
  filter is a prefix match), returning id, running state, and port bindings.
- `run_image(...)` ensures the image (`ensure_image` pulls if
  `inspect_image` fails), builds port bindings (host port `None` = random,
  host IP `127.0.0.1`), env, and volume binds (named volumes are prefixed via
  `Workspace::volume_name`; path-shaped hosts become bind mounts resolved
  against the workspace), creates and starts the container, then reads back
  the assigned ports and maps them to declared names.
- `build_and_run(...)` shells out to `docker build -t eph-<short_id>-<svc>`
  then delegates to `run_image`.
- `exec_in_container(...)` runs a health-check command via the exec API and
  returns the exit code.
- `stop_container` / `remove_container` / `remove_volume` are best-effort
  helpers (`remove_volume` treats a 404 as success).

**`ServiceManager`** owns the `Workspace`, a `DockerClient`, and the loaded
`ServiceState`:

- `start_services` is the entry point and runs in two phases. Phase 1 walks
  the targets in start order (role topological order in roles mode;
  declaration order with `run=` services last in legacy mode), running each
  service's `pre-start` hooks just before bringing it to a healthy state with
  `create_service`, then saves state. Phase 2 runs every target's
  `post-start` hooks with the resolved environment. A `resolved` map, seeded
  from `status` and grown as each service comes up, is threaded through both
  phases so a `pre-start` hook sees the services already up and phase 2
  reuses the same snapshot. `start_all` is a thin wrapper with an empty
  filter. A `skip_hooks` flag (CLI `--skip-hooks`) short-circuits both hook
  phases.
- `create_service` is the idempotent core that produces a healthy
  `RunningService` (no hooks). For `run=` services it first probes the
  tracked PID (a native liveness check via `proc::is_alive`) to avoid
  spawning duplicates. For Docker services it checks for an existing
  container: running means reuse, stopped means restart, absent means create
  fresh via the matching source path.
- `run_all_pre_start` / `run_all_post_start` run every service's hooks in one
  pass; `eph dev`, which drives the backing/foreground split by hand, uses
  them to bracket its startup (`pre-start` up front, then `post-start` once
  the foregrounded app is healthy).
- `wait_for_healthy` polls `exec_in_container` (image/dockerfile) on a 1s
  interval under a `tokio::time::timeout`; no health check means a 500 ms
  sleep. `start_shell_command` and `start_compose` have their own host-side
  platform-shell health-check loops (compose default 60s).
- `stop_service` takes the loaded `EphFile` and a snapshot of running
  services, runs `pre-stop` with the resolved environment (a failure is
  **propagated**, aborting teardown, unless `--skip-hooks`), then stops by
  source type, then runs `post-stop` against the same pre-teardown snapshot
  (also propagated on failure, but the service is already stopped, so it will
  not re-run on a later `down`). Stopping goes by source type: Docker (stop,
  optionally remove), `run` (graceful terminate via `proc::terminate`, wait,
  then `proc::force_kill`), or compose (`docker compose down`). For `run`,
  teardown targets the whole process tree the shell spawned, not just the
  wrapper PID: on Unix the shell is spawned in its own process group
  (`process_group(0)`) and terminate/kill signal the group with `killpg`
  (`SIGTERM` then `SIGKILL`, race-free), falling back to a `sysinfo`
  descendant walk only for legacy non-grouped state; on Windows, which has no
  signals and no reattachable process group, they walk and hard-terminate the
  descendant tree. `stop_all` and `clean` snapshot running services once up
  front and thread `skip_hooks` through; `stop_all` clears state.
- `resolve_env_vars` / `command_env` / `hook_env` build the resolved
  environment shared by `eph env`, `eph run`, and the lifecycle hooks.
- `clean` stops and removes everything, removes per-workspace named volumes
  (skipping bind mounts), clears state, and deletes the state directory,
  returning a `CleanSummary`.
- `status` reconciles persisted state against live containers and tracked
  PIDs, returning only what is actually running.

**State persistence:** `ServiceState { services: HashMap<String,
ServiceStateEntry>, auto_ports: ... }`, serialized as pretty JSON to
`<state_dir>/state.json`. `ServiceStateEntry { backend, ports }`, where
`backend` is a typed `Backend` enum: `Container { id }` for `image=` and
`dockerfile=`, `Process { pid, identity }` for `run=`, or
`Compose { project }` for compose. The PID is the addressable handle for
teardown; `identity` is what `eph system prune` checks to avoid signaling a
reused PID. `load` migrates a legacy file that used a stringly-typed
`container_id` (a bare id, `pid:<n>`, or `compose:<project>`) plus a separate
`processes` map. `auto_ports` records the ports allocated for `port=auto`
declarations so they can be reused on the next `up` while still free.

**`RunningService`** is the runtime handle returned to callers: `host()`
(always `localhost`), `port()` (the `default` port, else any),
`named_port(name)`. It is pure connection info; the backend handle lives only
in the persisted `ServiceStateEntry`.

## proc

`src/proc.rs` (crate-internal) hides the platform split for everything
host-process shaped: `shell_command(cmd)` builds the `sh -c` / `cmd /C`
invocation, spawning places the child in its own process group on Unix,
`is_alive` probes liveness (group probe on Unix, process-table lookup on
Windows), and `terminate` / `force_kill` implement graceful-then-forced
teardown of the whole tree. `ProcessIdentity` captures the launch-time
identity (used by prune to refuse to kill a reused PID).

## prune

`src/prune.rs` scans `<dirs::data_local_dir()>/eph/*` rather than resolving
the current workspace. Metadata-backed state is pruned only when the recorded
workspace path is missing, is empty, or is no longer a directory. Legacy state
has no path to check, so it is skipped unless the CLI passes
`--compatibility-v042`; even then, the state directory name must look like an
8-hex workspace short ID.

System prune removes Docker resources by namespace prefix
(`eph-<short_id>-`), so it does not need the original `.eph` or compose file.
It removes containers first, then volumes, networks, images, and the state
directory. For `run=` services it reads `state.json` and terminates only a
PID whose current `ProcessIdentity` exactly matches the saved identity; old
entries without identity, and mismatches, become warnings.

## env

`src/env.rs` is the pure rendering half of `eph env` (the workspace lookup
and interpolation live in `main.rs`):

- `render(vars, format)` dispatches to `render_export` / `render_fish` /
  `render_json` and errors on an unknown format.
- `render_export` emits `export NAME="value"`; `render_fish` emits
  `set -gx NAME "value"`; `render_json` emits a pretty JSON object.
- `escape_bash` escapes `\ " $ `` ` ``; `escape_fish` escapes `\ " $` (fish
  does not treat backticks specially in double quotes). Newlines are
  preserved. The unit tests pin these exact strings.

## skills

`src/skills.rs` embeds each `skills/<slug>/SKILL.md` with `include_str!` and
implements `eph skills install` (write into `.claude/skills/` and
`.agents/skills/` at the git repo root, never clobbering local edits without
`--force`), `eph skills check` (byte-compare, non-zero exit on drift; CI runs
this), and `eph skills list`.

## update

`src/update.rs` implements `eph update`: resolve the latest GitHub release,
download the platform archive, verify it against the release `checksums.txt`,
and swap the binary in place with `self-replace`. It also implements the
passive nag: commands read a cached latest-release lookup (never blocking),
and a detached `__update-check` invocation (a hidden subcommand) refreshes
the cache at most once a day. `EPH_REPO` / `EPH_BASE_URL` override the
source; `EPH_NO_UPDATE_CHECK` silences the nag.

## main

`src/main.rs` defines the `clap` `Cli`/`Commands`, initializes `tracing`
(debug if `--verbose`, else info) writing to **stderr** so stdout stays clean
for `eph env`, and dispatches to `cmd_*` functions. Each `cmd_*` resolves the
workspace, loads and parses `.eph` (`load_eph_file`), and drives the relevant
`ServiceManager` calls. `cmd_env` and `cmd_run` resolve the environment
through `service::resolve_env_vars` (and `ServiceManager::command_env` for
`cmd_run`), the same builder the lifecycle hooks use; `cmd_run` then execs the
given command with that environment and propagates its exit code. The
`eph dev` loop and its `--watch` restarts live here too, driving the library
through the same `ServiceManager` calls; `src/watch.rs` supplies the
debounced, gitignore-style file watching.
