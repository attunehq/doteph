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

| Flag | Description |
|------|-------------|
| `--skip-hooks` | Bring services up healthy but do not run their `post-start` hooks. |

```sh
eph up                 # all services
eph up postgres redis  # just these two
eph up --skip-hooks    # start everything but skip post-start (e.g. migrations)
```

- Idempotent: a running service is reused; a stopped-but-present container is
  restarted; otherwise a fresh container is created.
- Pulls/builds images as needed and waits for each `healthcheck`. Once **every**
  service started by this `up` is healthy, `post-start` hooks run -- deferring
  them to this second phase means a hook can reference any other service's
  assigned port. Hooks run on **every** `eph up` (fresh create or restart), so
  they should be idempotent; a failing `post-start` aborts the `up`. See
  [Core Concepts](concepts.md#the-service-lifecycle).
- Hooks run with eph's resolved environment injected -- the same variables
  `eph env` prints, plus `EPH_*` metadata and the service's own `env.X` values.
  See [The `.eph` file](eph-file.md#lifecycle-hooks).
- Prints each started service and its assigned host port.
- An unknown service name is an error (`unknown service: <name>`).

## `eph down [--rm] [SERVICE...]`

Stop services. With no arguments, stops all; with names, stops only those. Runs
`pre-stop` hooks first -- a failing `pre-stop` hook aborts the `down` and leaves
the service running so you can fix the hook and retry (see
[The `.eph` file](eph-file.md#lifecycle-hooks)).

| Flag | Description |
|------|-------------|
| `-r`, `--rm` | Also remove the stopped containers (not just stop them). |
| `--skip-hooks` | Stop without running `pre-stop` hooks (escape hatch for a broken hook). |

```sh
eph down               # stop all, keep containers
eph down --rm          # stop all and remove containers
eph down postgres      # stop just postgres
eph down --skip-hooks  # stop without running pre-stop hooks
```

Without `--rm`, containers and their data remain for a fast restart. With
`--rm`, containers are removed (named-volume data is kept); the next `eph up`
creates fresh containers.

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

| Flag | Description |
|------|-------------|
| `--skip-hooks` | Tear everything down without running `pre-stop` hooks. |

```sh
eph clean
eph clean --skip-hooks   # reset even if a pre-stop hook is broken
```

```
Workspace cleaned:
  Services stopped and removed: 3
  Named volumes removed: 2
  Persisted state: removed
```

> This **deletes the data** in named volumes. Bind mounts (host paths) are not
> touched.

> Like `eph down`, `clean` runs each service's `pre-stop` hooks first, and a
> failing hook aborts the reset before anything is removed. If a broken
> `pre-stop` hook is wedging `clean`, pass `--skip-hooks` to reset anyway.

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

## `eph run <CMD>...`

Run a command in the workspace root with eph's resolved environment already set,
without `eval`-ing anything. The command's environment is the same variables
`eph env` prints, plus the `EPH_*` metadata variables
(see [The `.eph` file](eph-file.md#lifecycle-hooks)).

```sh
eph run ./scripts/seed.sh            # the script sees DATABASE_URL, EPH_*, ...
eph run psql "$DATABASE_URL"         # $DATABASE_URL expanded by YOUR shell
eph run sh -c 'psql "$DATABASE_URL" < dump.sql'   # use sh -c for shell features
```

- The command is executed directly, **not** through a shell, so eph does not
  expand `$VAR`, globs, or pipes in the arguments. Wrap it in `sh -c '...'` when
  you need shell features driven by eph's injected variables.
- Resolution works exactly like `eph env`: placeholders for services that are
  not running are left unresolved, so run `eph up` first.
- `eph run` exits with the command's own exit code.
- Unlike `post-start`, `eph run` executes every time you invoke it -- use it for
  repeatable operations (seeding, resets, ad-hoc queries).

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

## `eph skills install [--dir DIR] [--force]`

Install the agent skills bundled into the `eph` binary into the repository, so a
coding agent working in this checkout discovers how to use `eph` (drive `eph up`,
load `eph env`, tear down). The skills are embedded in the binary, so this is
offline and self-contained.

| Flag | Description |
|------|-------------|
| `--dir DIR` | Skills directory to install into, relative to the repo root. Repeatable. Defaults to `.claude/skills` and `.agents/skills`. |
| `--force` | Overwrite an existing skill file even if it was edited locally. |

```sh
eph skills install
```

```
  created: .claude/skills/using-eph/SKILL.md
  created: .agents/skills/using-eph/SKILL.md

Commit these files so your agents discover them on checkout.
```

- The target is the **git repository root** containing the current directory (so
  the skills land at the top of the repo regardless of where you run it); it
  falls back to the current directory when you are not inside a git repo.
- A file that already matches what the binary would write is reported as
  `unchanged`. One that differs is left untouched and reported as `skipped`
  unless you pass `--force`, so a local edit is never clobbered silently.
- Commit the written files. Re-run `eph skills install` (or `--force`) after
  upgrading `eph` to pull in any updated skill text.

## `eph skills check [--dir DIR]`

Verify the installed skills match the binary's embedded source, without changing
anything. Prints one line per file and **exits non-zero** if any is missing or
has drifted, so CI can run it as a drift guard.

```sh
eph skills check
```

```
  up to date: .claude/skills/using-eph/SKILL.md
  up to date: .agents/skills/using-eph/SKILL.md
```

The rendered skill is deterministic and version-independent, so a checked-in copy
stays byte-stable across `eph` upgrades that do not change the skill text: this
check goes red only on a real drift (a hand edit, or a stale install left behind
when the skill source changed), not on every release.

## `eph skills list`

List the skills bundled into this `eph` binary, with the version they ship in.

```sh
eph skills list
```

## Commands that do not exist (by design)

The list above is the complete command set. A couple of things people look for
are deliberately delegated elsewhere:

- **There is no `eph logs`.** Inspect a service's logs with the `docker` CLI
  using the name from `eph info`: `docker logs eph-<short_id>-<service>`.
- **There is no `eph init` or scaffolder.** Create the `.eph` file by hand (see
  [Getting Started](getting-started.md)) and validate it with `eph check`.
