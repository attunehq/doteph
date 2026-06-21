# Internals

A module-by-module map of the source, so you can find where a change belongs.
For the rationale behind these structures, see [Architecture](architecture.md).

## Module layout

```
src/
  main.rs        CLI front end (clap) + cmd_* dispatch glue. Not public API.
  lib.rs         Library crate root; re-exports the public API.
  parser.rs      .eph -> AST, plus interpolation.
  workspace.rs   Workspace resolution, IDs, naming, paths.
  service.rs     Docker client wrapper + ServiceManager + persisted state.
  env.rs         Rendering resolved env vars for shell eval.
tests/
  common/mod.rs  Shared test helpers.
  integration.rs Happy-path lifecycle tests against a live Docker daemon.
  stress.rs      Heavyweight, concurrent, multi-service suite (#[ignore]'d).
```

The public API is whatever `src/lib.rs` re-exports:

```rust
pub use env::{escape_bash, escape_fish, render, render_export, render_fish, render_json};
pub use parser::{EphFile, Service, ServiceSource, parse, resolve_interpolations};
pub use service::{RunningService, ServiceManager};
pub use workspace::Workspace;
```

## parser

`src/parser.rs` turns `.eph` text into an `EphFile`.

**AST types:**

- `EphFile { env_vars: Vec<EnvVar>, services: HashMap<String, Service> }`
- `EnvVar { name, value }` - value stored verbatim, including `${...}`
  placeholders (resolved later, not here).
- `Service { name, source, ports, env, volumes, post_start, pre_stop,
  healthcheck, ready_timeout_secs, build_context, command_override }`.
- `ServiceSource` - `Image | Dockerfile | Compose | Command`. There is
  intentionally no "unset" variant: see below.
- `PortMapping { name: Option<String>, container_port: u16 }`.

**`parse(input) -> Result<EphFile>`** is a single line-oriented pass:

- Blank lines and `#`-leading lines are skipped. There is no inline-comment
  stripping, which is why a `#` after a value becomes part of the value.
- `[name]` opens a section; an empty `[]` is an error. Sections are kept in a
  `Vec<ServiceBuilder>` with a name->index map so re-opening a section name
  appends to the same builder and finalization order is deterministic.
- `key=value` splits on the first `=`; both sides are trimmed; a single matching
  pair of surrounding quotes is stripped (`strip_quotes`).
- Inside a section, `parse_service_property` interprets known keys (`image`,
  `dockerfile`, `compose`, `run`, `command`, `port`, `port.<name>`, `env.<KEY>`,
  `volume`, `post-start`, `pre-stop`, `healthcheck`, `ready-timeout`, `context`,
  `expose.<name>`). `port`/`ready-timeout`/named-port/expose values are parsed as
  numbers and error on bad input.
- An **unknown** key inside a section: if it looks like an env var name
  (`is_env_var_name` - non-empty SCREAMING_SNAKE_CASE), the section is ended and
  the key becomes a top-level `EnvVar` (with a `tracing::warn!`); otherwise it is
  a hard error. This is the deliberate "trailing env vars after sections" affordance
  that also swallows miscased property typos.
- After the pass, each `ServiceBuilder` is finalized with `ServiceBuilder::finish`,
  which **requires a source** - this is where the "service with no source" state
  is rejected, keeping it out of the returned `EphFile` entirely.

**`resolve_interpolations(input, resolver) -> String`** scans for `${...}`,
splits the content on the first `.` into `(service, property)`, and substitutes
`resolver(service, property)`. Anything the resolver declines (`None`), or a
placeholder with no `.`, is left verbatim. The resolver is supplied by the caller
(`cmd_env` in `main.rs`) and reads from running services.

## workspace

`src/workspace.rs` is pure path/ID logic, no I/O beyond canonicalization and the
data-dir lookup.

- `Workspace { path, id, short_id }`.
- `from_path` canonicalizes a directory and computes `id = sha256(path)`,
  `short_id = id[..8]`.
- `find_from_path` / `find_from_cwd` walk up from a start directory to the first
  ancestor containing a `.eph` file - the workspace resolution used by every
  command.
- Naming helpers: `container_prefix()` (`eph-<short_id>`), `container_name(svc)`,
  `volume_name(svc, vol)`, `eph_file_path()`.
- `state_dir()` returns `<dirs::data_local_dir()>/eph/<short_id>`.

## service

`src/service.rs` is the largest module: the Docker integration and the lifecycle
engine.

**`DockerClient`** wraps a `bollard::Docker`:

- `connect()` connects with local defaults and pings (this is the "is docker
  running?" check).
- `get_container(name)` filters by name and matches exactly (Docker's name filter
  is a prefix match), returning id, running state, and port bindings.
- `run_image(...)` ensures the image (`ensure_image` pulls if `inspect_image`
  fails), builds port bindings (host port `None` = random, `host_ip
  127.0.0.1`), env, and volume binds (named volumes are prefixed via
  `Workspace::volume_name`; `.`/`/` paths are bind mounts resolved against the
  workspace), creates and starts the container, then reads back the assigned
  ports and maps them to declared names.
- `build_and_run(...)` shells out to `docker build -t eph-<short_id>-<svc>` then
  delegates to `run_image`.
- `exec_in_container(...)` runs a health-check command via the exec API and
  returns the exit code.
- `stop_container` / `remove_container` / `remove_volume` are best-effort helpers
  (`remove_volume` treats a 404 as success).

**`ServiceManager`** owns the `Workspace`, a `DockerClient`, and the loaded
`ServiceState`:

- `start_service` is the idempotent core. For `run=` services it first probes the
  tracked PID (`kill -0`) to avoid spawning duplicates. For Docker services it
  checks for an existing container: running -> reuse; stopped -> restart (and
  **skip** `post-start`); absent -> create fresh via the matching source path and
  **run** `post-start`. `start_all` iterates and saves state.
- `wait_for_healthy` polls `exec_in_container` (image/dockerfile) on a 1s
  interval under a `tokio::time::timeout`; no health check means a 500 ms sleep.
  `start_shell_command` and `start_compose` have their own host-side `sh -c`
  health-check loops (compose default 60s).
- `stop_service` runs `pre-stop` (failures logged), then stops by source type:
  Docker (stop, optionally remove), `run` (SIGTERM, wait, SIGKILL), or compose
  (`docker compose down`). `stop_all` clears state.
- `clean` stops+removes everything, removes per-workspace named volumes (skipping
  bind mounts), clears state, and deletes the state directory, returning a
  `CleanSummary`.
- `status` reconciles persisted state against live containers and tracked PIDs,
  returning only what is actually running.

**State persistence:** `ServiceState { services: HashMap<String,
ServiceStateEntry>, processes: HashMap<String, u32> }`, serialized as pretty JSON
to `<state_dir>/state.json`. `ServiceStateEntry { container_id, ports }`.

**`RunningService`** is the runtime handle returned to callers: `host()` (always
`localhost`), `port()` (the `default` port, else any), `named_port(name)`. The
`container_id` field doubles as a backend tag: a real id, `pid:<n>` for `run`
services, or `compose:<project>` for compose.

## env

`src/env.rs` is the pure rendering half of `eph env` (the workspace lookup and
interpolation live in `main.rs`):

- `render(vars, format)` dispatches to `render_export` / `render_fish` /
  `render_json` and errors on an unknown format.
- `render_export` emits `export NAME="value"`; `render_fish` emits `set -gx NAME
  "value"`; `render_json` emits a pretty JSON object.
- `escape_bash` escapes `\ " $ `` ` `` `; `escape_fish` escapes `\ " $` (fish does
  not treat backticks specially in double quotes). Newlines are preserved. The
  unit tests pin these exact strings.

## main

`src/main.rs` defines the `clap` `Cli`/`Commands`, initializes `tracing` (debug
if `--verbose`, else info) writing to **stderr** so stdout stays clean for `eph
env`, and dispatches to `cmd_*` functions. Each `cmd_*` resolves the workspace,
loads and parses `.eph` (`load_eph_file`), and drives the relevant
`ServiceManager` calls. `cmd_env` builds the resolver closure over running
services and passes it to `parser::resolve_interpolations`.
