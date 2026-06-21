# Command Reference

Every `eph` command, its flags, and what it prints. All commands operate on the
workspace found by searching up from the current directory for a `.eph` file.

```
eph [--verbose] <command> [args]
```

## Global flags

| Flag | Description |
|------|-------------|
| `-v`, `--verbose` | Enable debug logging (written to stderr). |
| `-h`, `--help` | Print help. Works on subcommands too (`eph up --help`). |
| `-V`, `--version` | Print the version. |

Logging always goes to **stderr**; command output goes to **stdout**.

## `eph up [SERVICE...]`

Start services. With no arguments, starts all services in the `.eph` file. With
service names, starts only those.

```sh
eph up                 # all services
eph up postgres redis  # just these two
```

- Idempotent: a running service is reused; a stopped-but-present container is
  restarted; otherwise a fresh container is created.
- Pulls/builds images as needed, waits for each `healthcheck`, then runs
  `post-start` hooks. For `image`/`dockerfile` services those hooks run only on a
  fresh create (not when a stopped container is restarted); `run` services re-run
  them when the process is not already alive; `compose` services run them on
  every `eph up`. See
  [Core Concepts](concepts.md#the-service-lifecycle).
- Prints each started service and its assigned host port.
- An unknown service name is an error (`unknown service: <name>`).

## `eph down [--rm] [SERVICE...]`

Stop services. With no arguments, stops all; with names, stops only those. Runs
`pre-stop` hooks first.

| Flag | Description |
|------|-------------|
| `-r`, `--rm` | Also remove the stopped containers (not just stop them). |

```sh
eph down               # stop all, keep containers
eph down --rm          # stop all and remove containers
eph down postgres      # stop just postgres
```

Without `--rm`, containers and their data remain for a fast restart. With
`--rm`, containers are removed (named-volume data is kept); the next `eph up`
creates fresh containers and re-runs `post-start`.

> Two exceptions. **`compose`** services are always torn down with `docker
> compose down`, so `--rm` makes no difference for them. **`run`** services are
> always killed (there is no container to keep). Also note that a *targeted*
> `eph down <service>` updates only in-memory state - it does not rewrite
> `state.json` (only `eph down` with no arguments does); the leftover entry is
> harmlessly reconciled away by `eph status`. (The container itself is still only
> removed when you pass `--rm`.)

## `eph clean`

Full reset for the workspace. Stops and removes every service's container (or
Compose project / process), removes every **per-workspace named volume**, and
deletes the persisted state file.

```sh
eph clean
```

```
Workspace cleaned:
  Services stopped and removed: 3
  Named volumes removed: 2
  Persisted state: removed
```

> This **deletes the data** in named volumes. Bind mounts (host paths) are not
> touched.

## `eph status`

Show the workspace and which services are running. Reconciles saved state
against the live Docker daemon and tracked PIDs.

```sh
eph status
```

```
Workspace: /home/you/projects/myapp
ID: a1b2c3d4

Running services:
  postgres -> localhost:54321
  redis -> localhost:54322

Stopped services:
  minio
```

The `ID:` shown here is the short ID (`eph info` distinguishes the short ID from
the full SHA-256 workspace ID). If nothing is running, it lists the defined
services as stopped instead.

> All four service types are reconciled: `image`/`dockerfile` by container name,
> `run` by process ID, and `compose` by the Compose project's
> `com.docker.compose.project` label.

## `eph env [-f FORMAT]`

Print the top-level environment variables from the `.eph` file, with
`${service.property}` interpolations resolved against **running** services. For
shell `eval`.

| Flag | Values | Default |
|------|--------|---------|
| `-f`, `--format` | `export`, `fish`, `json` | `export` |

```sh
eval "$(eph env)"                # bash / zsh / sh
eph env -f fish | source         # fish
eph env -f json | jq -r .DATABASE_URL
```

- Only top-level variables are printed; service `env.*` values are not.
- Placeholders for stopped services are left unresolved. Run `eph up` first.
- Interpolation resolves against all running services, including `compose`
  services (their `expose.<name>` ports resolve as `${service.port.<name>}`).
- An unknown format is an error (`unknown format: ..., use: export, fish, json`).

See [Shell Integration](shell-integration.md) for details and escaping rules.

## `eph check`

Parse and validate the `.eph` file without touching Docker. Reports the
environment variables and services it found, or a parse error with a line
number.

```sh
eph check
```

```
Valid .eph file: /home/you/projects/myapp/.eph

Environment variables: 2
  DATABASE_URL
  REDIS_URL

Services: 2
  postgres (image: postgres:16-alpine)
  redis (image: redis:7-alpine)
```

## `eph info`

Print workspace metadata. Does not touch Docker.

```sh
eph info
```

```
Workspace path: /home/you/projects/myapp
Workspace ID: a1b2c3d4e5f6...        (full SHA-256)
Short ID: a1b2c3d4
Container prefix: eph-a1b2c3d4
.eph file: /home/you/projects/myapp/.eph
State directory: /home/you/.local/share/eph/a1b2c3d4
```

Use the container prefix and short ID to find this workspace's resources with
the `docker` CLI.

## Commands that do not exist (by design)

The list above is the complete command set. A couple of things people look for
are deliberately delegated elsewhere:

- **There is no `eph logs`.** Inspect a service's logs with the `docker` CLI
  using the name from `eph info`: `docker logs eph-<short_id>-<service>`.
- **There is no `eph init` or scaffolder.** Create the `.eph` file by hand (see
  [Getting Started](getting-started.md)) and validate it with `eph check`.
