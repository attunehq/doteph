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
pub use prune::{ConfirmationOutcome, PruneOptions, PruneReport, confirmation_outcome, prune};
pub use service::{Hooks, LogOptions, RunningService, ServiceManager, resolve_env_vars};
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

**`parse(input) -> Result<EphFile>`** strips a leading UTF-8 BOM, then makes a
single line-oriented pass tracking a `Context` (`TopLevel`, `Env`,
`RolesOrder`, or `Service(usize)`) that says how to interpret the current
line:

- Blank lines and `#`-leading lines are skipped. There is no inline-comment
  stripping, which is why a `#` after a value becomes part of the value.
- `[name]` opens a section; an empty `[]` is an error. `[env]` and
  `[roles_order]` are reserved names: `[env]` switches `Context` back to
  `Env` (top-level variables) rather than opening a service, and may repeat;
  `[roles_order]` switches to `Context::RolesOrder` (role edges). Any other
  name must match `is_valid_service_name` (`^[a-z][a-z0-9-]*$`) and must not
  repeat an earlier section; both are hard errors, and `reserved_section_hint`
  recognizes common misspellings of `[env]`/`[roles_order]` and names the
  correct one instead of treating the typo as an ordinary unknown section.
  Sections are kept in a `Vec<ServiceBuilder>` with a name-to-index map,
  populated once per section (a repeat is rejected, not appended to), so
  finalization order stays deterministic.
- `key=value` splits on the first `=`; both sides are trimmed; a single
  matching pair of surrounding quotes is stripped by `strip_quotes`, which
  only strips when the pair is unambiguous (>= 2 chars, matching outer quotes,
  no interior occurrence of that quote), so `"a" and "b"` and a bare `"` both
  pass through unchanged.
- In `Context::TopLevel` or `Context::Env`, every line is a top-level
  `EnvVar`: the key must satisfy `is_valid_env_name` (`^[A-Za-z_][A-Za-z0-9_]*$`)
  and must not repeat a name already seen in `env_lines`, a `HashMap` shared
  across the top-of-file block and every `[env]` section so the two forms
  share one duplicate check. Interpolation placeholders in the value are
  validated immediately with `scan_placeholders` (see below).
- In `Context::Service(index)`, `parse_service_property` interprets known keys
  (`image`, `dockerfile`, `compose`, `run`, `role`, `command`, `port`,
  `port.<name>`, `env.<KEY>`, `volume`, `pre-start`, `post-start`, `pre-stop`,
  `post-stop`, `healthcheck`, `ready-timeout`, `context`, `expose.<name>`).
  Single-valued properties go through `set_once` (source properties through
  `set_source`), which rejects a second occurrence rather than overwriting;
  hooks and `volume` push onto a `Vec` and accumulate. Most properties reject
  an empty value up front (`env.<KEY>` is the exception). `port`/`port.<name>`
  and `expose.<name>` are collected separately (`ServiceBuilder::ports` vs
  `expose`) and reconciled in `finish` (below). Numeric values (`port`,
  `ready-timeout`, named ports, expose) error on bad input; `port`/`port.<name>`
  additionally accept the literal `auto`.
- An **unknown** key inside a section is always a hard error listing every
  known property (`KNOWN_PROPERTIES`). If the key also looks like an
  environment variable name (`is_valid_env_name` and starts uppercase), the
  error additionally suggests `env.<key>=` (container) or moving it to
  `[env]` (shell) rather than silently reclassifying it, which is what the
  parser did before this rewrite.
- After the line-by-line pass, each `ServiceBuilder` is finalized with
  `ServiceBuilder::finish`, which **requires a source** (rejecting the "no
  source" state), rejects `port=auto`/`port.<name>=auto` on anything but a
  `run=` service, rejects `command=` on anything but `image=`/`dockerfile=`,
  and reconciles `ports` vs `expose` against the source (`port`/`port.<name>`
  on a `compose=` service, or `expose.<name>` on anything else, is an error).
- Every `${service.property}` placeholder collected by `scan_placeholders`
  during the pass (from top-level values and `env.<KEY>` values; **not** from
  `run=`, hook, or `healthcheck=` values, which are shell commands) is checked
  against the final service list once the whole file has been read, so a
  variable may reference a service defined later in the file. An unterminated
  `${`, a placeholder with no `.` splitting it into `service` and `property`,
  or a reference to a service that does not exist, is a parse error. A
  literal `${` is written `$${` and is skipped by the scanner entirely.
- Finally, `validate_roles` checks the role graph (linear or DAG form,
  desugared into one `RolesOrder`) against the services: every service must
  be tagged if any is, every tag must resolve, and the graph must be acyclic.

**`resolve_interpolations(input, resolver) -> String`** scans for `${...}`,
treating `$${` as an escaped literal `${`, splits the content on the first
`.` into `(service, property)`, and substitutes `resolver(service, property)`.
Anything the resolver declines (`None`), or a placeholder with no `.`, is left
verbatim; an unterminated `${` is copied through rather than repaired, since
`parse` already guarantees a parsed file has none. The resolver is supplied by
the caller (`cmd_env` in `main.rs` and `service::resolve_against`) and reads
from running
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
  host IP `127.0.0.1`), resolves each `env.<KEY>` value's `${service.property}`
  references against the services already running (the same `resolve_against`
  the `run=` path uses, so a container service's `env.DATABASE_URL=${postgres.port}`
  no longer ships as a literal placeholder), builds volume binds (named
  volumes are prefixed via `Workspace::volume_name`; path-shaped hosts become
  bind mounts resolved against the workspace), creates and starts the
  container, then reads back the assigned ports and maps them to declared
  names.
- `build_and_run(...)` shells out to `docker build -t eph-<short_id>-<svc>`
  then delegates to `run_image`.
- `exec_in_container(...)` runs a health-check command via the exec API and
  returns the exit code.
- `stop_container` / `remove_container` / `remove_volume` are best-effort
  helpers (`remove_volume` treats a 404 as success), each returning whether it
  actually stopped/removed/found something so `clean` can report measured
  counts instead of counting declared services.
- `containers_with_prefix(prefix)` / `volumes_with_prefix(prefix)` list every
  container/volume whose name starts with `eph-<short_id>-`, regardless of
  whether eph's own state knows about them. `clean` uses these for a final
  sweep, since a service renamed before state recorded it, or a resource left
  behind by a crash before state was written, would otherwise survive a
  "full reset".

**`ServiceManager`** owns the `Workspace`, a `DockerClient`, and the loaded
`ServiceState`:

- `start_services` opens the workspace's `WorkspaceLock` (below), reloads
  state under it, then runs in two phases over the targets in start order
  (role topological order in roles mode; declaration order with `run=`
  services last in legacy mode). Phase 1 walks the targets one at a time,
  running each service's `pre-start` hooks (via `run_service_pre_start`) just
  before bringing it to a healthy state with `create_service`, then saves
  state to disk immediately, before moving to the next target: if a later
  target's hook or creation fails, whatever already started is still on disk
  for `eph down` to find, rather than living only in the discarded in-memory
  state. Phase 2 runs every target's `post-start` hooks with the fully
  resolved environment. A `resolved` map, seeded from `status` and grown as
  each service comes up, is threaded through both phases so a `pre-start`
  hook sees the services already up and phase 2 reuses the same snapshot.
  `start_all` is a thin wrapper with an empty filter. A `hooks: Hooks`
  parameter (`All`, `None`, or `PreStartOnly`) selects which phases run:
  `eph up` passes `Hooks::from_skip_flag(--skip-hooks)` (`All` or `None`);
  `eph dev` passes `PreStartOnly` for its backing services so pre-start stays
  interleaved per service exactly as under `up`, while post-start is deferred
  until the foregrounded app is also healthy (see `run_all_post_start` below).
- `run_service_pre_start` / `run_service_post_start` run one service's hooks
  against an already-resolved `running` map; they are the shared core behind
  both `start_services`' phases and the two public single-purpose methods
  below.
- `run_pre_start_for(eph, name)` runs one named service's `pre-start` hooks
  against the services currently up. `eph dev` calls this for the foreground
  app immediately before spawning it, so the hook is interleaved the same way
  `eph up` interleaves every other service's: it sees the backing services
  already up, never its own not-yet-assigned port. There is no
  `run_all_pre_start` any more; interleaving replaced the old front-loaded
  pass where every service's `pre-start` ran before anything existed.
- `run_all_post_start(eph)` runs every declared service's `post-start` hooks
  once, in start order, against a single resolved snapshot. `eph dev` calls
  it after the backing services and the foreground app are all up, so a hook
  can still reference any service's assigned port.
- `create_service` is the idempotent core that produces a healthy
  `RunningService` (no hooks). For `run=` services it first probes the
  tracked PID via `Backend::process_is_alive` (a liveness check that also
  compares the recorded `ProcessIdentity`, not just PID presence, so a PID
  reused by an unrelated process is never mistaken for the original) to avoid
  spawning duplicates. For Docker services it checks for an existing
  container: running means reuse, stopped means restart, absent means create
  fresh via the matching source path.
- `wait_for_healthy` polls `exec_in_container` (image/dockerfile) on a 1s
  interval under a `tokio::time::timeout`; no health check means a 500 ms
  sleep. `start_shell_command` and `start_compose` have their own host-side
  platform-shell health-check loops (compose default 60s).
- `stop_all` opens the `WorkspaceLock`, reloads state, snapshots running
  services, then stops every declared service (via `stop_service`, in the
  reverse of start order) followed by every `orphaned_state_entries` (via
  `stop_orphan`), saving state once at the end. There is no wholesale
  `state.services.clear()`: each `stop_service` / `stop_orphan` call removes
  only its own entry as it actually tears the service down, so a service that
  fails to stop is not silently forgotten from state. `stop_selected` is the
  same shape restricted to an explicit subset (a filtered `eph down`, or
  `eph dev` tearing down only what it brought up).
- `stop_service` runs `pre-stop` with the resolved environment (a failure is
  **propagated**, aborting teardown, unless `--skip-hooks`), stops by source
  type, then runs `post-stop` against the same pre-teardown snapshot (also
  propagated on failure, but the service is already stopped, so it will not
  re-run on a later `down`). Stopping goes by source type: Docker (stop,
  optionally remove), `run` (graceful terminate via `proc::terminate`, wait,
  then `proc::force_kill`, skipped entirely when `process_is_alive` is
  already false), or compose (`docker compose down`, invoked only when
  `running` or recorded state has the project up; a failure here is a real
  error and propagates, no longer swallowed). For `run`, teardown targets the
  whole process tree the shell spawned, not just the wrapper PID: on Unix the
  shell is spawned in its own process group (`process_group(0)`) and
  terminate/kill signal the group with `killpg` (`SIGTERM` then `SIGKILL`,
  race-free), falling back to a `sysinfo` descendant walk only for legacy
  non-grouped state; on Windows, which has no signals and no reattachable
  process group, they walk and hard-terminate the descendant tree. Returns
  `bool`: whether something was actually stopped or removed, so callers (in
  particular `clean`) can report measured counts instead of counting declared
  services. `stop_orphan` is the same teardown for a state entry whose
  section was renamed or deleted from the `.eph` file: it works entirely from
  the recorded `Backend` (no `Service` definition exists any more, so no
  hooks run and no volumes are known), and also returns `bool`.
  `orphaned_state_entries` is the plain diff (state keys minus declared
  service names) both `stop_all` and `clean` use to find them.
- `resolve_env_vars` / `command_env` / `hook_env` build the resolved
  environment shared by `eph env`, `eph run`, and the lifecycle hooks.
  `hook_env` resolves the owning service's own `env.X` values'
  `${service.property}` references before overlaying them, so a hook like
  `post-start` sees the same resolved values the service itself was created
  with rather than a raw placeholder.
- `clean` opens the `WorkspaceLock`, reloads state, tears down every declared
  service (`stop_service`) and every `orphaned_state_entries` (`stop_orphan`),
  removes each stopped service's per-workspace named volumes (skipping bind
  mounts), then sweeps Docker directly for any container or volume still
  carrying the `eph-<short_id>-` prefix (`containers_with_prefix` /
  `volumes_with_prefix`) to catch a service renamed before state recorded it,
  or a resource left behind by a crash before state was written. It clears
  in-memory state and deletes the state directory, and returns a
  `CleanSummary` whose counts (`services_removed`, `volumes_removed`,
  `state_removed`) reflect only what was actually removed, not what the
  `.eph` file declares.
- `status` reconciles persisted state against live containers and tracked
  PIDs (via `Backend::process_is_alive`, identity-checked), returning only
  what is actually running.

**State persistence:** `ServiceState { services: HashMap<String,
ServiceStateEntry>, auto_ports: ... }`, serialized as pretty JSON to
`<state_dir>/state.json`. `ServiceStateEntry { backend, ports }`, where
`backend` is a typed `Backend` enum: `Container { id }` for `image=` and
`dockerfile=`, `Process { pid, identity }` for `run=`, or
`Compose { project }` for compose. The PID is the addressable handle for
teardown; `identity` (`Backend::process_is_alive`) is what every liveness
check (`status`, `create_service`'s dedup, `stop_service`, and `eph system
prune`) compares against the live process before treating a PID as still
belonging to the service that recorded it, so a PID reused by an unrelated
process is never mistaken for it or signaled. `load` migrates a legacy file
that used a stringly-typed `container_id` (a bare id, `pid:<n>`, or
`compose:<project>`) plus a separate `processes` map; a file that fails to
parse at all (a crash mid-write, manual editing, disk corruption) is
quarantined to `state.json.corrupt` with a warning rather than treated as
fatal, and `load` returns fresh empty state so `eph clean` still has
something to reset. `save` writes atomically: it serializes to a sibling
`state.json.tmp` and renames it over `state.json`, so a crash mid-write can
never leave a truncated file behind. `auto_ports` records the ports allocated
for `port=auto` declarations so they can be reused on the next `up` while
still free.

**`WorkspaceLock`** serializes the state-mutating commands (`start_services`,
`stop_all`, `stop_selected`, `clean`) for one workspace. It is an
`fd_lock::RwLock` write guard on `<state_dir's parent>/<dir_name>.lock`, a
file next to (not inside) the state directory: `clean` deletes the state
directory while holding this lock, and on Windows a directory cannot be fully
removed while an open, locked file still lives inside it. Being an OS
advisory lock rather than a marker file, it releases automatically when the
holding process exits for any reason, so a killed `eph up` can never wedge
the next command the way a stale marker file would. Every lock-holding method
reloads `ServiceState` from disk immediately after acquiring the lock, since
another command may have run and changed it between this `ServiceManager`'s
construction and the lock actually being acquired.

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
identity, and `identity_matches` compares it against a live PID's current
identity; both are used everywhere a recorded PID's liveness is checked
(`ServiceManager` and `eph system prune` alike) to refuse to act on a PID that
has been reused by an unrelated process.

## prune

`src/prune.rs` scans `<dirs::data_local_dir()>/eph/*` rather than resolving
the current workspace. Metadata-backed state is pruned only when the recorded
workspace path is missing, is empty, or is no longer a directory. Legacy state
has no path to check, so it is skipped unless the CLI passes
`--compatibility-v042`; even then, the state directory name must look like an
8-hex workspace short ID.

Before removing anything for a stale-pathed workspace, `prune_workspace`
checks it for signs of life: any container matching the `eph-<short_id>-`
prefix that Docker reports as running, or any `run=` service whose recorded
PID's `ProcessIdentity` still matches. A workspace that was only moved or
renamed (rather than truly deleted) reads exactly like a dead one by path
alone, so this guard is what keeps that case from having its live containers
and processes force-killed; either count above zero reports the workspace as
skipped (with a reason naming what is still alive) instead of pruning it.
`PruneOptions::force_live` bypasses the guard entirely, restoring the
unconditional old behavior. The same check runs during `--dry-run`, so the
preview `eph system prune` shows before prompting matches what a real run
would actually do.

Once a workspace clears the liveness guard (or `force_live` is set), removal
proceeds by namespace prefix (`eph-<short_id>-`), so it does not need the
original `.eph` or compose file: containers first, then volumes, networks,
images, and the state directory. For `run=` services it reads `state.json`
and terminates only a PID whose current `ProcessIdentity` exactly matches the
saved identity; old entries without identity, and mismatches, become
warnings.

`prune()` itself is made mutually exclusive across concurrent invocations by
`open_prune_lock`, an `fd_lock::RwLock` write guard held on `<state_root>/
prune.lock` for the call's duration. This replaced an earlier `PruneLock` that
`create_new`'d the lock file and relied on `Drop` to delete it: a crash skipped
`Drop`, so the file (and the lock) outlived the process, wedging every
subsequent prune, including `--dry-run`, until someone deleted it by hand. The
OS-level advisory lock releases the instant the holding process exits, crash
or not.

`eph system prune`'s confirmation prompt is decided by the pure
`confirmation_outcome(would_remove, yes, stdin_is_terminal) -> ConfirmationOutcome`
(`Proceed`, `Prompt`, or `RequireYes`), kept separate from the actual
`std::io::stdin()` read so the decision is unit-testable without a real
terminal: nothing to remove or `--yes` passed means proceed silently; an
interactive terminal means show the "Remove these resources? [y/N]" prompt;
a non-interactive stdin with something to remove and no `--yes` means refuse
with an actionable error rather than silently doing nothing or silently
proceeding. The CLI (`cmd_system_prune` in `main.rs`) runs `prune` once as a
dry-run preview to decide and display what would happen, resolves the
confirmation, then (if confirmed) runs `prune` again for real.

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
given command with that environment and propagates its exit code. `cmd_up`
passes `Hooks::from_skip_flag(--skip-hooks)` straight through to
`start_services`.

The `eph dev` loop and its `--watch` restarts live here too, driving the
library through the same `ServiceManager` calls; `src/watch.rs` supplies the
debounced, gitignore-style file watching. `dev_bring_up` reproduces `eph up`'s
hook interleaving by hand for the backing/foreground split: it starts the
backing services with `Hooks::PreStartOnly` (their `pre-start` hooks run
per-service, `post-start` deferred), then calls `run_pre_start_for` for the
foreground app immediately before `start_foreground` spawns it, then finally
`run_all_post_start` once the foreground app is healthy too, so a seed hook
can reference the app's own assigned port. `eph dev --skip-hooks` skips all
three calls, matching `eph up --skip-hooks` / `eph down --skip-hooks`.

`cmd_system_prune` runs `eph::prune` once as a dry-run preview (so the
confirmation prompt, when shown, describes exactly what is about to happen),
resolves `eph::confirmation_outcome` against `io::stdin().is_terminal()` and
the `-y`/`--yes` flag, then (once confirmed, or immediately when nothing
needs confirming) runs `eph::prune` again for real.
