---
title: "Core Concepts"
summary: "Workspaces, isolation, automatic ports, persisted state, and the service lifecycle."
order: 2
---

# Core Concepts

This page explains the model behind `eph`: workspaces, isolation, automatic
ports, persisted state, the service lifecycle, and the split between dependency
services and the app you are building. Once these six ideas click, the commands
and the file format are obvious.

## Workspaces

A **workspace** is any directory that contains a `.eph` file.

When you run an `eph` command, it searches the current directory and then walks
**up** through parent directories until it finds a `.eph` file, the same way
git finds a repository. The directory that holds the file is the workspace, and
every command operates on that workspace, so `eph status` works from any
subdirectory of your project.

If no `.eph` file is found in the current directory or any parent, the command
fails with `no .eph file found`.

All relative paths and shell commands in your `.eph` file (volumes,
`dockerfile=`, `compose=`, `run=`, health checks, and lifecycle hooks) resolve
and execute **from the workspace root**, not from wherever you happen to run
the command.

## Isolation

Each workspace gets its own eph-managed resource namespace, including two
checkouts of the same repository. Direct containers receive separate names,
volumes, and automatic host ports. Fixed `run=` ports and bindings declared in
a Compose file remain explicit configuration and can still conflict.

Isolation is keyed on a **workspace ID**: the SHA-256 hash of the workspace's
absolute (canonicalized) path. The first 16 hex characters form the **short
ID** that namespaces everything `eph` creates:

```
~/projects/app/      ->  short ID a1b2c3d4e5f60718  ->  eph-a1b2c3d4e5f60718-postgres
~/projects/app-v2/   ->  short ID e5f60718293a4b5c  ->  eph-e5f60718293a4b5c-postgres
```

| Resource | Name |
|----------|------|
| Container | `eph-<short_id>-<service>` |
| Named volume | `eph-<short_id>-<service>-<volume>` |
| Built image (`dockerfile=`) | `eph-<short_id>-<service>` |
| Compose project (`compose=`) | `eph-<short_id>-<service>` |

Because the ID comes from the path, the two checkouts above get different
container names, different volumes, and different ports. If the state root
contains an 8-character namespace whose metadata matches the full workspace
ID, eph uses that namespace until `eph clean`. An 8-character state directory
that cannot be verified blocks workspace construction so eph cannot start a
second namespace beside unknown resources.

Run `eph info` to see the ID, short ID, container prefix, and paths for the
current workspace.

## Automatic ports

For each `port=` you declare, `eph` asks Docker to
publish the container port on a **random free host port**, bound to
`127.0.0.1`. This means:

- **Direct container services avoid fixed host-port conflicts.** Each creation
  asks Docker for an available port.
- **Nothing is exposed to your local network.** Services are bound to loopback
  only.
- **The real port changes between container creations**, so never hardcode it.
  Reference it symbolically instead (next section) and let `eph env` fill in
  the current value.

One exception: `run=` (non-container) services bind whatever port their process
binds. With a numeric `port=`, `eph` reports the declared value as-is; with
`port=auto`, `eph` allocates a free port and injects it into the process. See
[Running Your App](run-your-app.md#portauto).

Compose services own their port publishing rules. An `expose.<alias>=` entry
discovers the host port selected by the Compose file; eph does not change its
bind address.

## Interpolation: connecting your app to the ports

Because ports are dynamic, your environment variables reference services
symbolically:

```ini
[postgres]
image=postgres:16-alpine
port=5432

[redis]
image=redis:7-alpine
port=6379

[env]
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
REDIS_URL=redis://localhost:${redis.port}
```

When you run `eph env`, each `${...}` is replaced using the **currently
running** services:

| Reference | Resolves to |
|-----------|-------------|
| `${service.port}` | The assigned host port (single-port services) |
| `${service.port.name}` | A named port (multi-port services) |
| `${service.host}` | Always `localhost` |

If a service is not running, `eph env` clears the affected variable in shell
formats, appends a failing shell statement, warns on stderr, and exits nonzero.
JSON omits the affected key and also exits nonzero. `eph run` refuses to launch
with an incomplete top-level environment. Run `eph up` first so every
reference resolves.

All four service types resolve once running: `eph` finds `image` and
`dockerfile` services by container name, `run` services by their tracked
process, and `compose` services by their Compose project label. Details in
[Shell Integration](shell-integration.md).

## Persisted state

When `eph` starts services, it records what it started (container IDs, assigned
ports, and process PIDs) in a small `state.json` file, with workspace metadata
beside it:

| Platform | Location |
|----------|----------|
| Linux | `~/.local/share/eph/<short_id>/state.json` |
| macOS | `~/Library/Application Support/eph/<short_id>/state.json` |
| Windows | `%LOCALAPPDATA%\eph\<short_id>\state.json` |

When a file declares stop or clean hooks, state also stores the last parsed
teardown commands, their top-level and service environments, service order, and
backend family. System prune needs that snapshot when a worktree has already
been deleted. Environment values may include development credentials, so protect
`state.json` like `.eph`; `eph clean` and system prune remove it with the rest of
the workspace state.

Set `EPH_STATE_ROOT` to an absolute path to override the parent directory (the
`eph` above `<short_id>`) for every workspace. Relative values are rejected so
state cannot move when a command runs from a different directory. The override
is useful for relocating state or giving a test harness a throwaway root.

State is why `eph status` and `eph env` answer instantly, why assigned ports
survive a terminal restart, and why `eph` knows which containers and volumes
belong to this workspace.

`state.json` is written after every individual service starts, not once at
the end of `eph up`: if a later service's `pre-start` hook or creation fails,
whatever already started is still on disk, so `eph down` can find and stop
it instead of leaking it. The write itself is atomic (a temp file, renamed
over the real one), so an interrupted write does not replace state with a
truncated file
behind. If `state.json` is still unreadable (hand-edited, corrupted by
something outside eph), the next command quarantines it to
`state.json.corrupt`, warns, and continues with empty state rather than
aborting; recover a `run=` service's PID by hand if it needs stopping, since
that is the one thing the quarantine cannot recover on its own (containers are
found again from Docker by name).

`eph up`, `eph down`, and `eph clean` on a given workspace serialize against
each other through an OS-level file lock, so two overlapping commands never
race the same `state.json` or double-spawn a service; the second command
waits for the first (printing a notice while it does) rather than
proceeding against stale state. The lock is released by the operating system
the instant the holding process exits, crash included, so a killed `eph up`
can never wedge a later command.

Two commands manage state directories in bulk: `eph clean` deletes the state
directory for the current workspace along with its services and data, and
[`eph system prune`](command-reference.md#eph-system-prune---dry-run---force---compatibility-v042---force-non-empty---force-live--y---yes)
scans **all** state directories and removes leftovers for workspaces whose
directory has since been deleted (a worktree you removed, for example). A
successful `eph up` checks for exactly that situation in other workspaces (a
cheap filesystem scan, never Docker) and prints a one-line note on stderr
pointing at `eph system prune` when it finds any, so stale state does not sit
unnoticed until you happen to run prune yourself.

## The service lifecycle

Bringing a service up is **idempotent when its effective configuration still
matches**. `eph up` fingerprints the source, immutable image, ports, resolved
environment, volumes, health settings, build context, and command before it
chooses a path:

1. **Already running and matching**: the service is reused, then any declared
   health check is rerun.
2. **Stopped but still present and matching** (after `eph down`): the existing
   resource is restarted and checked. Fast, and the data is still there.
3. **Not present**: a fresh container is created, pulling or building the image
   if needed.
4. **Configuration drifted**: the old resource is removed through the backend
   type that created it, then the requested configuration is created. This also
   handles source changes such as `run=` to `image=`.

That is the container story (`image` and `dockerfile`). The other two sources
have the same fingerprint gate: a `run` service includes its complete resolved
process environment, and Compose includes its exact delegated configuration.
Dockerfile services build through Docker's cache on every `up`, then use the
resulting image ID to detect effective context changes. A failed start is
removed before `up` returns, so the next attempt cannot adopt a broken leftover.

### Hooks bracket the lifecycle

Each service can declare six hooks: `pre-start`, `post-start`, `pre-stop`,
`post-stop`, `pre-clean`, and `post-clean`. During `eph up`, each service's
`pre-start` hooks run just before it is created (codegen, generated config), and
once **every** service in the
`up` is healthy, all `post-start` hooks run in a second phase (migrations,
seeds). Deferring `post-start` this way means a hook can reference any other
service's assigned port. Teardown mirrors it: `pre-stop` before a service stops
(backup, drain), `post-stop` after. `eph clean` wraps that teardown with
`pre-clean` before the stop hooks and `post-clean` after the backend and managed
volumes are removed. Clean hooks also run for an already-stopped service.

Two rules matter here; the full contract lives in
[The `.eph` File](eph-file.md#lifecycle-hooks):

- **`pre-start` and `post-start` run on every `eph up`**, not only on fresh
  creation. Write them to be idempotent: a migration that no-ops when applied,
  an `INSERT ... ON CONFLICT` seed. For one-off work, use
  [`eph run`](command-reference.md#eph-run-cmd) instead.
- **A failing hook aborts the command** rather than being skipped silently.
  `--skip-hooks` is the escape hatch on `up`, `down`, and `clean`.

System prune is the exception because it is a cross-workspace recovery command.
It runs stop hooks only when it stops a live service and clean hooks whenever it
cleans a snapshotted service. A current valid `.eph` wins over saved hooks; when
the file or worktree is unavailable, prune uses the saved snapshot. Hook failures
become warnings and cleanup continues, while resource-removal failures remain
fatal. Dry runs do not execute hooks.

### Three levels of teardown

| Command | Stops | Removes container | Removes named volumes (data) | Removes state |
|---------|:-----:|:-----------------:|:----------------------------:|:-------------:|
| `eph down` | yes | no | no | clears entries |
| `eph down --rm` | yes | yes | no | clears entries |
| `eph clean` | yes | yes | **yes** | deletes directory |

`eph down` keeps containers and data for a fast restart. `eph down --rm`
removes containers but keeps named-volume data, forcing a fresh create next
time. `eph clean` is the full reset: it **deletes the data in named volumes**,
so use it when you want to start over completely.

Three footnotes to the table:

- Bind mounts (host paths like `./seed` or `C:\data`) are never deleted by
  `eph clean`. Only Docker named volumes are.
- `compose` services are torn down with `docker compose down` in every case, so
  `--rm` makes no difference for them. Compose services cannot declare `.eph`
  volumes, and `eph clean` does not remove volumes defined inside the Compose
  file. See [Defining Services](services.md#how-compose-services-differ).
- Teardown works from **recorded state**, not just the sections currently in
  your `.eph` file. A bare `eph down` (no service names) and `eph clean` both
  also stop and remove anything `state.json` remembers starting under a name
  that is no longer in the file, so renaming or deleting a service's section
  does not orphan its container. (A targeted `eph down <service>` only
  accepts names that still exist in the file, so it cannot reach a renamed
  entry by its old name; use the bare form to sweep those up.) `eph clean`
  additionally reports **measured** counts, what it actually stopped or
  removed rather than the number of services declared, so a workspace that
  never started anything reports zeros; it also sweeps any leftover Docker
  container or volume still carrying the workspace's `eph-<short_id>-` name
  prefix, in case something exists that neither the file nor the recorded
  state knows about.

## Dependency services vs the app

Most stacks split in two. **Dependency services** (databases, caches, queues,
mail catchers) are stable, slow to warm up, and fine to leave running. The
**first-party app** is what you restart constantly and want to control
precisely.

`eph` lets you name that split. Tag each service with a `role=` (say, `dep`
and `app`) and declare the order with `roles_order=dep,app`, which reads "app
depends on dep". You get two things:

- **Start order follows the dependency graph.** `eph up` brings the `dep` tier
  up and healthy first, then the app, so the app's `DATABASE_URL` resolves the
  moment it starts. Teardown runs in reverse.
- **Tiers are addressable.** `eph up --role dep` starts just the dependency
  services, for example to prewarm databases from a coding agent's
  session-start hook without launching the app. `eph up --role app` starts the
  app and pulls its dependencies up with it.

Roles are opt-in. A file with no `role=` and no `roles_order` uses implicit
ordering: services start in declaration order with `run=` services last.
The full rules are in [The `.eph` File](eph-file.md#roles-and-ordering), and
the prewarming workflow is in
[Recipes](recipes.md#prewarm-dependency-services-on-claude-code-session-start).

## Next

You have the model. [The `.eph` File](eph-file.md) gives you the complete
format, or jump ahead to [Defining Services](services.md) for ready-to-use
service definitions.
