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

Each workspace is isolated from every other workspace, including two checkouts
of the same repository. That is the point of the tool: run the same project in
several directories at once, with no shared ports and no shared data.

Isolation is keyed on a **workspace ID**: the SHA-256 hash of the workspace's
absolute (canonicalized) path. The first 8 hex characters (the **short ID**)
namespace everything `eph` creates:

```
~/projects/app/      ->  short ID a1b2c3d4  ->  eph-a1b2c3d4-postgres
~/projects/app-v2/   ->  short ID e5f6g7h8  ->  eph-e5f6g7h8-postgres
```

| Resource | Name |
|----------|------|
| Container | `eph-<short_id>-<service>` |
| Named volume | `eph-<short_id>-<service>-<volume>` |
| Built image (`dockerfile=`) | `eph-<short_id>-<service>` |
| Compose project (`compose=`) | `eph-<short_id>-<service>` |

Because the ID comes from the path, the two checkouts above get different
container names, different volumes, and different ports. They never see each
other's data.

Run `eph info` to see the ID, short ID, container prefix, and paths for the
current workspace.

## Automatic ports

You never pick host ports. For each `port=` you declare, `eph` asks Docker to
publish the container port on a **random free host port**, bound to
`127.0.0.1`. This means:

- **No port conflicts, ever.** Not between workspaces, and not with other
  software on your machine.
- **Nothing is exposed to your local network.** Services are bound to loopback
  only.
- **The real port changes between container creations**, so never hardcode it.
  Reference it symbolically instead (next section) and let `eph env` fill in
  the current value.

One exception: `run=` (non-container) services bind whatever port their process
binds. With a numeric `port=`, `eph` reports the declared value as-is; with
`port=auto`, `eph` allocates a free port and injects it into the process. See
[Running Your App](run-your-app.md#portauto).

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

If a service is not running, its placeholders are left **untouched**, so the
unresolved reference stays visible instead of silently becoming empty. Run
`eph up` before `eph env`.

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

State is why `eph status` and `eph env` answer instantly, why assigned ports
survive a terminal restart, and why `eph` knows which containers and volumes
belong to this workspace.

`state.json` is written after every individual service starts, not once at
the end of `eph up`: if a later service's `pre-start` hook or creation fails,
whatever already started is still on disk, so `eph down` can find and stop
it instead of leaking it. The write itself is atomic (a temp file, renamed
over the real one), so a crash mid-write can never leave a truncated file
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
[`eph system prune`](command-reference.md#eph-system-prune---dry-run---compatibility-v042---force-live--y---yes)
scans **all** state directories and removes leftovers for workspaces whose
directory has since been deleted (a worktree you removed, for example).

## The service lifecycle

Bringing a service up is **idempotent**. `eph up` takes whichever of three
paths applies:

1. **Already running**: the service is reused. Nothing restarts.
2. **Stopped but still present** (after `eph down`): the existing container is
   restarted. Fast, and the data is still there.
3. **Not present**: a fresh container is created, pulling or building the image
   if needed.

That is the container story (`image` and `dockerfile`). The other two sources
have their own idempotency: a `run` service is reused if its tracked process is
alive and respawned otherwise, and a `compose` service delegates to
`docker compose up -d`, which is itself idempotent.

### Hooks bracket the lifecycle

Each service can declare four hooks: `pre-start`, `post-start`, `pre-stop`, and
`post-stop`. During `eph up`, each service's `pre-start` hooks run just before
it is created (codegen, generated config), and once **every** service in the
`up` is healthy, all `post-start` hooks run in a second phase (migrations,
seeds). Deferring `post-start` this way means a hook can reference any other
service's assigned port. Teardown mirrors it: `pre-stop` before a service stops
(backup, drain), `post-stop` after.

Two things to internalize now; the full rules live in
[The `.eph` File](eph-file.md#lifecycle-hooks):

- **`pre-start` and `post-start` run on every `eph up`**, not only on fresh
  creation. Write them to be idempotent: a migration that no-ops when applied,
  an `INSERT ... ON CONFLICT` seed. For one-off work, use
  [`eph run`](command-reference.md#eph-run-cmd) instead.
- **A failing hook aborts the command** rather than being skipped silently.
  `--skip-hooks` is the escape hatch on `up`, `down`, and `clean`.

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
  `--rm` makes no difference for them, and `eph clean` removes only the named
  volumes declared in your `.eph` file, not volumes internal to the Compose
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

Roles are opt-in. A file with no `role=` and no `roles_order` behaves as it
always has: services start in declaration order with `run=` services last.
The full rules are in [The `.eph` File](eph-file.md#roles-and-ordering), and
the prewarming workflow is in
[Recipes](recipes.md#prewarm-dependency-services-on-claude-code-session-start).

## Next

You have the model. [The `.eph` File](eph-file.md) gives you the complete
format, or jump ahead to [Defining Services](services.md) for ready-to-use
service definitions.
