# Internals

This is a code map for contributors. It describes the current boundaries and
invariants without duplicating implementation details that are easier to read in
the source. See [Architecture](architecture.md) for the rationale.

## Module layout

```text
src/
  main.rs        clap front end and command dispatch
  system_prune.rs system prune CLI options, confirmation, and reporting
  watch.rs       binary-side watcher for eph dev --watch
  lib.rs         public library surface
  parser.rs      .eph parser, checked types, roles, interpolation
  workspace.rs   workspace discovery, IDs, names, state paths
  hooks.rs       persisted teardown hooks, environments, command execution
  service.rs     lifecycle engine, Docker adapter, persisted state
  proc.rs        platform shell and process identity/control
  prune.rs       stale-workspace discovery and resource removal
  env.rs         shell and JSON environment rendering
  skills.rs      bundled skill install/check/list
  update.rs      release lookup, verification, and self-replacement
tests/
  common/mod.rs  integration helpers
  integration.rs CLI and live lifecycle coverage
  integration/system_prune_hooks.rs focused prune-hook scenarios
  stress.rs      ignored heavyweight concurrency and backend-change cases
```

`src/lib.rs` is the authority for the public API. Keep `main.rs` as an adapter;
reusable parsing, state, and lifecycle behavior belongs in the library.

## parser

`parser::parse` converts `.eph` text into an `EphFile` whose invalid states have
already been rejected. The checked model centers on:

- `EphFile { env_vars, services, roles_order }`, with `IndexMap` preserving
  declaration order.
- `Service`, which always has exactly one `ServiceSource`.
- `PortMapping::{Fixed, Auto, Compose}`. Fixed and Compose ports contain
  `NonZeroU16`; Compose keeps the interpolation alias separate from the target
  Compose service.
- `Healthcheck { command, timeout_secs: Option<NonZeroU64> }`, which makes a
  timeout without a health check unrepresentable after parsing.
- `CommandOverride`, which owns parsed argv. Runtime code consumes `argv()` and
  never reparses the original string.

The parser uses a `Context` state machine: `TopLevel`, `Env`, `RolesOrder`, or a
service builder. A section remains active until another header appears. A bare
variable below a service is therefore rejected with guidance to move it above
the first section or into `[env]`.

Boundary checks include:

- exactly one service source and source-specific properties;
- unique sections, scalar properties, ports, roles, and environment names;
- non-empty values except `env.KEY=` and root role dependency lists;
- non-zero ports and readiness timeouts;
- `EPH_*` reserved in every environment scope, case-insensitively;
- complete role graphs with no empty comma segments, duplicates, unknown
  dependencies, or cycles;
- semantic interpolation: only `host`, `port`, and declared `port.<name>`
  properties exist, and a bare port must be unambiguous.

Top-level and service environment values are scanned as eph interpolation.
Health checks preserve ordinary shell forms such as `${PORT}` while collecting
dotted eph references such as `${api.port}` for semantic validation. `run=` and
hook command strings remain shell-owned.

`resolve_interpolations` performs substitution and preserves the `$${` escape.
`resolve_interpolations_tracked` also returns references the resolver declined.
The lifecycle layer uses that tracked result to prove an environment is complete
before crossing an execution boundary.

## workspace

`Workspace::from_path` canonicalizes the workspace path and hashes it with
SHA-256. The first 16 hex characters form `short_id`. If an 8-character state
directory carries metadata for the same full workspace ID, eph uses that
namespace until `eph clean`; an unverifiable 8-character directory blocks
workspace construction.

Naming helpers derive container, image, volume, and Compose project namespaces
from `short_id`. `state_root()` uses the platform local-data directory unless
`EPH_STATE_ROOT` supplies a non-empty absolute path. Relative overrides are
rejected. `save_metadata()` writes the canonical workspace path for prune.

## service

`ServiceManager` owns a `Workspace`, `DockerClient`, and `ServiceState`.
State-mutating commands acquire `WorkspaceLock`, reload state under the lock,
and save after each material transition.

### State and runtime fingerprints

`ServiceState` stores:

- live `ServiceStateEntry { backend, ports }` records;
- remembered auto ports for `run=` services;
- `ServiceConfigRecord { fingerprint, backend }` records that survive `down`.

`Backend` is the teardown authority: container ID, identity-checked process PID,
or Compose project. Keeping it beside the fingerprint lets a source change be
removed through the backend that actually created the old resource.

`prepare_service` builds a canonical `RuntimeConfigSpec` and hashes its stable
JSON representation with SHA-256. The spec includes source, immutable image ID,
ports, sorted resolved environment, volumes, health settings, build context, and
command argv. `run=` adds its complete top-level, metadata, service environment,
and assigned self port. A dependency port change therefore changes downstream
fingerprints.

Dockerfile services build on every `up` through Docker's cache; the resulting
image ID enters the fingerprint. Reconciliation discards a mismatched backend,
state without a fingerprint, and orphan config records. Matching reused
resources rerun declared health checks.

### Startup and failure cleanup

`start_services` walks services in role-topological order, or declaration order
with `run=` last when the file has no role graph. Before anything runs, the
ports of every `run=` service the invocation will start are reserved
(`reserve_command_ports`): recorded in `auto_ports` as the spawn path's reuse
candidate and exposed to resolution as provisional running entries, so a
`${svc.port}` reference to a managed app resolves in hook and container
environments evaluated before the app spawns. `eph dev` passes its foreground
app as a reserve-ahead name for the same reason. Each `pre-start` then runs
immediately before its service. After every selected service is healthy,
`post-start` runs in the same order.

Image and Dockerfile services use Bollard for container create/start/exec.
Compose delegates to the Docker CLI and queries the exact
`expose.<alias>=<compose-service>:<port>` mapping. `run=` uses the platform shell
and records a process identity with its PID.

Every background process exit during the startup grace period is a failed
`up`. Auto-port processes retry on fresh ports; fixed-port and portless processes
fail immediately. Foreground `eph dev` apps use the same retry rule while
inheriting stdin, stdout, and stderr. A failed container, Compose project, or
process start is discarded and the cleaned state is saved before returning.

### Strict environment resolution

`resolve_against_strict` returns either a fully resolved string or structured
`UnresolvedReference` values. `resolve_env_vars_strict` returns a complete
environment or `UnresolvedEnvironment`, which retains safe values plus the
affected variables and references.

Execution boundaries use strict resolution:

- service environments before container, Compose, or process launch;
- health-check commands before execution;
- lifecycle hook environments;
- `eph run`.

No child receives a raw unresolved eph placeholder. `eph env` is the rendering
case: shell formats emit unsets followed by a failure statement, JSON omits
affected keys, and the command exits non-zero.

### Teardown and logs

`down` stops recorded backends in reverse order and preserves config records so
stopped resources can be reconciled on the next `up`. `down --rm` removes direct
containers. Compose always uses `compose down`. `clean` also removes declared
named volumes, sweeps namespaced leftovers, and deletes workspace state. Each
declared service's `pre-clean` runs before its stop hooks, while `post-clean`
runs after its backend and managed volumes are removed. These clean-only hooks
are not gated on liveness, so a clean after `down` still runs them.
Every state-mutating lifecycle entry point refreshes the optional teardown hook
snapshot under `WorkspaceLock` before its first side effect, including when
`--skip-hooks` suppresses execution for that invocation.

Logs use Docker or Compose for container backends and captured files for `run=`.
The all-services path streams concurrently and tags lines; the one-service path
stays raw for piping.

## proc

`proc.rs` centralizes native behavior:

- `shell_command`: `sh -c` on Unix, `cmd /C` on Windows;
- `spawn_captured`: the detached background `run=` spawn (null stdin, log-file
  stdout/stderr);
- direct command spawning for `eph run`;
- process identity capture and verification;
- whole-tree termination.

Unix shells lead a process group so teardown can signal the group. Windows uses
a `sysinfo` descendant snapshot because a daemonless later invocation cannot
reattach to a named Job Object. Every lifecycle path refuses to signal a PID
unless its current identity matches the recorded one.

On Windows, `spawn_captured` calls `CreateProcessW` directly with a
`PROC_THREAD_ATTRIBUTE_HANDLE_LIST` so the service inherits only its three
stdio handles. The std/tokio spawn passes `bInheritHandles=TRUE`, which copies
every inheritable handle in eph (including the stdout/stderr pipes a capturing
caller handed eph) into the long-lived service tree, and the caller's pipe
read then blocks until the tree dies. `disinherit_std_handles`, called at
startup by `main`, clears the inherit flag on eph's own std handles so
shorter-lived std/tokio children (hooks, health checks, the update worker)
cannot re-leak them either. The `proc` module docs cover the mechanism in
depth.

## prune

`prune` scans state directories with 16-hex or 8-hex names,
classifies missing or empty workspace paths, and discovers namespaced Docker and
process resources. It lists each Docker resource type once per pass, then
partitions that snapshot by workspace namespace. `--force-non-empty` also
classifies existing non-empty paths as candidates. Live resources block every
candidate unless `--force-live` is set.
An 8-hex state directory without workspace metadata requires
`--compatibility-v042`. A global lock prevents concurrent prune runs. A
destructive pass also holds every candidate's `WorkspaceLock` before loading
the shared Docker inventory, which serializes prune against `up`, `down`,
`clean`, and foreground `dev` startup without returning to per-workspace Docker
listings. The binary-side `system_prune` module resolves `--force` into all
three scope overrides plus confirmation bypass before it calls the library.
For hook execution, prune prefers a readable, valid current `.eph` and falls
back to the typed snapshot in `ServiceState`. It constructs the pre-teardown
port environment once, runs clean hooks for every snapshotted service and stop
hooks only for live services, then uses recorded backend truth to stop them.
Hook failures append warnings and the prune continues; resource failures still
return an error. A missing worktree uses the state directory as the hook working
directory so PATH-only cleanup commands remain usable.

## env

`env.rs` renders export, fish, PowerShell, and JSON formats. Shell escaping is
format-specific. `render_with_unsets` accepts affected variable names so failed
resolution clears stale values before the emitted script returns failure. JSON
preserves declaration order and excludes unresolved keys.

## skills

`skills.rs` embeds `skills/<slug>/SKILL.md`, installs deterministic copies into
`.claude/skills` and `.agents/skills`, and compares installed files against the
embedded source. The provenance marker is replaced during installation. CI runs
the check so committed agent guidance cannot drift.

## update

`update.rs` resolves the latest GitHub release, verifies the selected archive
against `checksums.txt`, extracts the platform binary, and replaces the running
executable. Release comparisons use semantic versions; development builds do not
produce misleading update nags.

## main and watch

`main.rs` parses CLI arguments, resolves the workspace and `.eph`, connects to
Docker only for commands that need it, and delegates to library APIs. `eph run`
splits its argv before clap so every token after `run` belongs to the child and
exits with the child's native status.

Role flags select whole dependency or dependent closures. Positional service
selection in roles mode selects only that service from its own role, plus whole
dependency roles for `up` or dependent roles for `down`.

`watch.rs` normalizes paths relative to the workspace, ignores `.git`, filters
configured globs, and coalesces filesystem bursts for `eph dev --watch`.

## Tests

Unit tests cover pure parsing, resolution, state, fingerprint, rendering, and
process helpers. `tests/integration.rs` owns CLI and live lifecycle behavior.
`tests/stress.rs` contains ignored heavyweight concurrency and backend-change
scenarios. Prefer real processes and Docker resources over mocks; keep fixtures
local to the behavior they prove.
