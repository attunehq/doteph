# Core Concepts

This page explains the model behind `eph`. Once these five ideas click -
workspaces, isolation, automatic ports, persisted state, and the lifecycle -
the commands and the file format are obvious.

## Workspaces

A **workspace** is any directory that contains a `.eph` file.

When you run an `eph` command, it searches the current directory and then walks
**up** through parent directories until it finds a `.eph` file. The directory
that holds it is the workspace, and every command operates on that workspace. So
you can run `eph status` from a deep subdirectory of your project and it still
finds the right services.

If no `.eph` file is found in the current directory or any parent, the command
fails with `no .eph file found`.

All relative paths and shell commands in your `.eph` file (volumes,
`dockerfile=`, `compose=`, `run=`, health checks, `post-start`/`pre-stop` hooks)
are resolved and executed **from the workspace root**, not from your current
directory.

## Isolation

Each workspace is isolated from every other workspace, even two checkouts of the
same repository. This is what lets you run the same project in several
directories at once without conflicts.

Isolation is keyed on a **workspace ID**: the SHA-256 hash of the workspace's
absolute (canonicalized) path. The first 8 hex characters - the **short ID** -
are used for naming:

```
~/projects/app/      ->  short ID a1b2c3d4  ->  eph-a1b2c3d4-postgres
~/projects/app-v2/   ->  short ID e5f6g7h8  ->  eph-e5f6g7h8-postgres
```

Everything `eph` creates is namespaced by the short ID:

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
publish the container port on a **random free host port**, bound to `127.0.0.1`.

This means:

- **No port conflicts**, ever - not between workspaces, not with other software.
- Services are reachable from your machine but **not exposed to the local
  network** (they are bound to loopback only).
- The real port changes between runs, so you should never hardcode it. Reference
  it through interpolation instead (see below), and let `eph env` fill in the
  current value.

There is one exception: `run=` (non-container) services bind whatever port their
process binds. `eph` does not remap those - the declared `port=` is reported
as-is. See
[Defining Services](services.md#run---shell-command-non-docker-services).

## Interpolation: connecting your app to the ports

Because ports are dynamic, your environment variables reference services
symbolically:

```ini
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
REDIS_URL=redis://localhost:${redis.port}
```

When you run `eph env`, each `${...}` is replaced using the **currently running**
services:

| Reference | Resolves to |
|-----------|-------------|
| `${service.port}` | The assigned host port (single-port services) |
| `${service.port.name}` | A named port (multi-port services) |
| `${service.host}` | Always `localhost` |

If a service is not running, its placeholders are left **untouched** so the
unresolved reference stays visible rather than silently becoming empty. So run
`eph up` before `eph env`. Full details in
[Shell Integration](shell-integration.md).

> Interpolation and `eph status` currently track `image`, `dockerfile`, and
> `run` services. `compose` services are **not** tracked after `up`, so their
> `expose` ports do not resolve through `eph env`. See
> [Defining Services](services.md#compose---delegate-to-docker-compose).

## Persisted state

When `eph` starts services, it records what it started - container IDs, the
assigned ports, and any process PIDs - in a small `state.json` file:

| Platform | Location |
|----------|----------|
| Linux | `~/.local/share/eph/<short_id>/state.json` |
| macOS | `~/Library/Application Support/eph/<short_id>/state.json` |
| Windows | `%LOCALAPPDATA%\eph\<short_id>\state.json` |

State is why `eph status` and `eph env` work instantly without re-inspecting
everything, why the assigned ports survive a terminal restart, and why `eph`
knows which containers and volumes belong to this workspace. `eph clean` deletes
this directory.

## The service lifecycle

Starting a service is **idempotent** and has three paths:

1. **Already running** -> `eph up` detects it and reuses it. Nothing restarts.
2. **Stopped but still present** (after `eph down`) -> the existing container is
   restarted. This is fast, and it reuses the existing data. **`post-start`
   hooks do *not* run again** on this path.
3. **Not present** -> a fresh container is created, the image pulled or built if
   needed, the health check awaited, and then **`post-start` hooks run**.

That second point is the one that surprises people: migrations or seed scripts
in `post-start` run when the container is *created*, not every time you `up`. To
force them to run again, recreate the container with `eph down --rm` (then `eph
up`) or `eph clean`. See
[Troubleshooting](troubleshooting.md#post-start-hooks-did-not-run-again).

This lifecycle (paths 1-3 above) applies to `image` and `dockerfile` services.
The other two source types behave a little differently:

- **`run`** services are tracked by process ID. `eph up` re-runs `post-start`
  whenever the process is not already alive (it is skipped if it is).
- **`compose`** services delegate to `docker compose up -d` on every `eph up`
  (which is itself idempotent), and their `post-start` hooks run **every** time.

The three levels of teardown:

| Command | Stops | Removes container | Removes named volumes (data) | Removes state |
|---------|:-----:|:-----------------:|:----------------------------:|:-------------:|
| `eph down` | yes | no | no | clears entries |
| `eph down --rm` | yes | yes | no | clears entries |
| `eph clean` | yes | yes | **yes** | deletes file |

`eph down` keeps containers and data for a fast restart. `eph down --rm` removes
containers but keeps named-volume data (and forces a fresh create next time).
`eph clean` is the full reset and **deletes the data in named volumes** - use it
when you want to start completely fresh.

> Bind mounts (host paths starting with `.` or `/`) are never deleted by
> `eph clean` - only Docker named volumes are.

> The table describes `image`/`dockerfile` services. **`compose` services are an
> exception**: both `eph down` and `eph down --rm` run `docker compose down`,
> which removes the compose containers either way. `eph clean` removes only the
> named volumes declared with `volume=` in your `.eph` file; volumes declared
> inside a Compose file are left to `docker compose`.

## Next

Now that you have the model, see [The `.eph` File](eph-file.md) for the complete
format, or jump to [Defining Services](services.md) for ready-to-use service
definitions.
