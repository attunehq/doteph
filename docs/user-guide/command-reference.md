# Command Reference

Every `eph` command, its flags, and what it prints. Most commands operate on the
workspace found by searching up from the current directory for a `.eph` file.
`eph system prune` is global: it scans eph's state root for deleted workspaces.

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
| `--role ROLE` | Bring up this role and everything it depends on (its forward/dependency closure). Repeatable. Requires a `roles_order`; combines with any SERVICE names. |
| `--skip-hooks` | Bring services up healthy but do not run their `pre-start` or `post-start` hooks. |

```sh
eph up                 # all services
eph up postgres redis  # just these two
eph up --role dep      # the dep tier and its dependencies (e.g. prewarm the database)
eph up --role app      # the app plus every role it depends on
eph up --skip-hooks    # start everything but skip pre-start/post-start (e.g. codegen, migrations)
```

- `--role ROLE` (repeatable) selects a role and its **dependency closure**: the
  role plus every role it transitively depends on, since a role cannot run without
  the roles below it. `--role app` with `roles_order=dep,app` starts both `dep` and
  `app`; `--role dep` starts only `dep`. It combines with positional SERVICE names,
  starting the union. Using `--role` on a file that defines no `roles_order` is an
  error saying so. See [Roles and ordering](eph-file.md#roles-and-ordering).

- Idempotent: a running service is reused; a stopped-but-present container is
  restarted; otherwise a fresh container is created.
- Each service runs its `pre-start` hooks just before it is created (the place
  for prep it depends on, such as codegen), then pulls/builds images as needed
  and waits for each `healthcheck`. Once **every** service started by this `up`
  is healthy, `post-start` hooks run -- deferring them to this second phase means
  such a hook can reference any other service's assigned port. Hooks run on
  **every** `eph up` (fresh create or restart), so they should be idempotent; a
  failing `pre-start` aborts the `up` before its service starts, and a failing
  `post-start` aborts the `up`. See
  [Core Concepts](concepts.md#the-service-lifecycle).
- Hooks run with eph's resolved environment injected -- the same variables
  `eph env` prints, plus `EPH_*` metadata and the service's own `env.X` values.
  See [The `.eph` file](eph-file.md#lifecycle-hooks).
- Prints each started service and its assigned host port.
- An unknown service name is an error (`unknown service: <name>`).

## `eph down [--rm] [SERVICE...]`

Stop services. With no arguments, stops all; with names, stops only those. Each
service runs its `pre-stop` hooks before it stops and its `post-stop` hooks after
(see [The `.eph` file](eph-file.md#lifecycle-hooks)). A failing `pre-stop` aborts
the `down` and leaves the service running so you can fix the hook and retry; a
failing `post-stop` aborts the rest of the teardown, but its own service is
already stopped.

| Flag | Description |
|------|-------------|
| `--role ROLE` | Stop this role and everything that depends on it (its reverse/dependent closure), in reverse start order. Repeatable. Requires a `roles_order`; combines with any SERVICE names. |
| `-r`, `--rm` | Also remove the stopped containers (not just stop them). |
| `--skip-hooks` | Stop without running `pre-stop` or `post-stop` hooks (escape hatch for a broken hook). |

```sh
eph down               # stop all, keep containers
eph down --rm          # stop all and remove containers
eph down postgres      # stop just postgres
eph down --role dep    # stop the dep tier and everything that depends on it
eph down --skip-hooks  # stop without running pre-stop/post-stop hooks
```

- `--role ROLE` (repeatable) selects a role and its **dependent closure**: the role
  plus every role that transitively depends on it, torn down in reverse start order,
  because a dependency cannot be removed while the roles that need it are still up.
  With `roles_order=dep,app`, `eph down --role dep` stops both `app` and `dep`. It
  combines with positional SERVICE names, and requires a `roles_order` (an error
  otherwise). `eph down` is otherwise absolute: it stops exactly what it targets,
  with no ownership logic. See [Roles and ordering](eph-file.md#roles-and-ordering).

Without `--rm`, containers and their data remain for a fast restart. With
`--rm`, containers are removed (named-volume data is kept); the next `eph up`
creates fresh containers.

> Two exceptions. **`compose`** services are always torn down with `docker
> compose down`, so `--rm` makes no difference for them. **`run`** services are
> always killed (there is no container to keep). A *targeted* `eph down <service>`
> persists the updated state, so the stopped services drop out of `state.json`
> immediately. (The container itself is still only removed when you pass `--rm`.)

## `eph clean`

Full reset for the workspace. Stops and removes every service's container (or
Compose project / process), removes every **per-workspace named volume**, and
deletes the persisted state file.

| Flag | Description |
|------|-------------|
| `--skip-hooks` | Tear everything down without running `pre-stop` or `post-stop` hooks. |

```sh
eph clean
eph clean --skip-hooks   # reset even if a pre-stop/post-stop hook is broken
```

```
Workspace cleaned:
  Services stopped and removed: 3
  Named volumes removed: 2
  Persisted state: removed
```

> This **deletes the data** in named volumes. Bind mounts (host paths) are not
> touched.

> Like `eph down`, `clean` runs each service's `pre-stop` hooks before stopping
> it and `post-stop` hooks after, and a failing hook aborts the reset. If a
> broken `pre-stop` or `post-stop` hook is wedging `clean`, pass `--skip-hooks`
> to reset anyway.

## `eph system prune [--dry-run] [--compatibility-v042]`

Cross-workspace prune for state left behind after worktrees are deleted. It
scans the eph state root, reads each workspace's recorded path, and removes
resources for workspaces whose path is gone or now an empty directory.

| Flag | Description |
|------|-------------|
| `--dry-run` | Print what would be removed without deleting Docker resources, processes, or state. |
| `--compatibility-v042` | Also prune state directories written by eph v0.4.2 and earlier, before workspace paths were recorded. |

```sh
eph system prune
eph system prune --dry-run
eph system prune --compatibility-v042
```

```text
System prune complete:
  a1b2c3d4 (missing workspace) - C:\Users\me\.codex\worktrees\1234\app
    containers: 2, volumes: 1, networks: 1, images: 1, run processes: 0, state dirs: 1

Totals:
  Containers: 2
  Volumes: 1
  Networks: 1
  Images: 1
  Verified run= processes: 0
  State directories: 1
```

System prune removes Docker resources by eph's workspace namespace
(`eph-<short_id>-...`), so it can remove direct containers, built images, named
volumes, Compose containers, and Compose networks even when the original `.eph`
or compose file is gone. It deletes the workspace state directory last.

For `run=` services, system prune kills only PIDs whose current process identity
matches the identity eph recorded when it launched the service. Legacy `run=`
state has no identity, and a mismatched PID may be a reused PID, so system prune
skips it and prints a warning. If a `run=` command deliberately detaches
grandchild processes outside the shell tree eph launched, system prune cannot
find those after the recorded shell tree is gone.

By default, v0.4.2-and-earlier state directories are skipped because older state
does not record the workspace path. `--compatibility-v042` prunes them by
`short_id` namespace only; directories whose names are not 8 hex digits are still
skipped. Use `--dry-run --compatibility-v042` first if you have old state on
disk.

## `eph dev [SERVICE] [--clean] [--watch GLOB]...`

Run the whole dev stack in the foreground, built for a Claude Desktop preview
server (see [Recipes](recipes.md#claude-desktop-preview-servers)). It brings
every service up (running `post-start` hooks, e.g. seeding), foregrounds a
`run=` service with eph's own stdin, stdout, and stderr wired through to it, and
stays attached until it is stopped. On stop it tears down only the services it
brought up itself, leaving any that were already running when it started (a
prewarmed dependency tier, typically) up.

| Flag | Description |
|------|-------------|
| `--clean` | On the final stop, tear the **whole** workspace down with `eph clean` (drop named volumes and their data), rather than the default of stopping only the services `eph dev` brought up and keeping the rest. |
| `--watch GLOB` | Restart the whole stack when a file matching GLOB changes. Repeatable; globs are relative to the workspace root with gitignore-style separators. |

```sh
eph dev            # foreground the sole run= service; eph down on stop
eph dev web        # foreground a specific run= service by name
eph dev --clean    # full reset (eph clean) on stop
eph dev --watch "**/*.rs" --watch "*.toml"   # restart on source changes
```

- **Setup** mirrors `eph up`, driving the backing/foreground split by hand: run
  every service's `pre-start` hooks up front (rather than interleaved per
  service), bring the backing services up and wait for health, foreground the
  chosen `run=` app, then run every service's `post-start` hooks. A failing
  service or hook aborts the command. `post-start` runs once the app is up so a
  seed can reach it, and (see `$PORT` below) before the preview port is opened.
- **Foreground**: the chosen `run=` service inherits eph's stdin, stdout, and
  stderr, so it is fully interactive and its output streams straight through
  (during `eph dev` that output is not also captured to its `eph logs` file).
  `eph dev` blocks until the app stops. With no `SERVICE`, the sole `run=`
  service is used; name one when the `.eph` defines several. A `.eph` with no
  `run=` service is an error. eph's own startup chrome goes to stderr so it does
  not mingle with the app's stdout.
- **`$PORT` override and the readiness gate**: when the environment sets `PORT` (a
  preview server's `autoPort` passes the host port it picked this way), `eph dev`
  keeps the app on its own internal `port=auto` and opens `$PORT` as a forwarding
  gate to it, but only *after* `post-start` hooks finish. Because the preview
  server watches `$PORT`, it cannot see the app as ready until seeding is done,
  instead of the moment the server can answer its health check. Give the
  foreground app `port=auto` and read its `${service.port}`; do not also pin a
  fixed port.
- **Teardown on stop**: a stop signal (the preview server's, or Ctrl-C) stops only
  the services `eph dev` brought up, then exits zero. `eph dev` snapshots what was
  already running at startup (a simple in-memory record, no persisted refcount) and
  leaves those services up, so a dependency tier a SessionStart hook prewarmed stays
  warm for the next command. With `--clean` it instead runs `eph clean` and
  bulldozes the whole workspace, volumes included. A hard kill (`SIGKILL` /
  `TerminateProcess`) cannot be caught, so it skips teardown and leaves the stack
  up, recoverable with `eph down`.
- **App exit**: if the foregrounded app exits on its own (a crash), `eph dev`
  leaves the backing services up and exits non-zero, so the preview server sees
  the dev server went down. The app's own output already streamed to your
  terminal, and `eph down` stops the rest.
- **`--watch`**: each `--watch` value is a glob (repeatable) matched against
  paths relative to the workspace root, using gitignore-style separators: `*`
  stays within a directory and `**` spans them, so `*.toml` matches a top-level
  `Cargo.toml` while `**/*.rs` matches a `.rs` file at any depth. When a matching
  file changes, eph tears down the services it brought up (running `pre-stop` and
  `post-stop` hooks) and brings them back up (running `pre-start` and `post-start`
  hooks), so a restart is a full down + up, not a bare process bounce, and every
  lifecycle hook fires just as it would on a manual restart. Only the services
  `eph dev` brought up are bounced: an adopted, already-running dependency tier
  stays hot across restarts. A restart always keeps volumes for speed, even under
  `--clean`; that reset is reserved for the final stop. Changes are debounced, so one save is one restart,
  and git's own churn under `.git` never triggers one. Without any `--watch` the
  stack never restarts.
  In watch mode an app that exits on its own (a crash) does not end the session:
  eph reports it and waits for the next change to restart, since editing is when
  the app is most likely to crash. That differs from plain `eph dev`, where the
  same exit is reported as a failure and ends the command so a preview server
  sees the dev server went down.

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

## `eph logs [SERVICE] [-f] [-n N]`

Show a service's logs. Works across every service type from one command: `run`
services read from the log file eph captures their output to; `image` /
`dockerfile` services proxy `docker logs`; `compose` services proxy
`docker compose logs`.

| Flag | Description |
|------|-------------|
| `-f`, `--follow` | Stream new output as it is produced (like `tail -f`); Ctrl-C to stop. Works with or without a `SERVICE`. |
| `-n`, `--tail N` | Show only the last `N` lines before printing/streaming. |

```sh
eph logs                      # every service interleaved, each line tagged [name]
eph logs -f                   # follow all services at once (Ctrl-C to stop)
eph logs worker               # just the worker service (untagged, raw)
eph logs -f worker            # follow worker
eph logs -n 50 postgres       # last 50 lines
```

- Logs are shown **even for a stopped service**, so a `run` service that died on
  startup still leaves an inspectable trace. (Its output is captured to
  `<state-dir>/logs/<service>.log`; see [`eph info`](#eph-info) for the state
  directory.)
- A `run` service's log file is truncated each time the service is freshly
  started, so it reflects the current run.
- With no `SERVICE`, every service is streamed concurrently and **interleaved**
  in arrival order (like `docker compose logs`), with each line prefixed by a
  right-aligned, color-coded `[name]` tag. Lines are emitted whole, so two
  services never interleave mid-line. A single `eph logs <service>` is untagged
  and passes the raw stream through. Colors are emitted only to a terminal and
  suppressed when `NO_COLOR` is set or output is piped.
- `eph clean` removes the captured log files along with the rest of the
  workspace state.

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

## `eph update [--check] [--force]`

Update `eph` to the latest GitHub release, replacing the running binary in place.
It is a native updater with no dependency on `curl` or a shell: it resolves the
latest published release, downloads the archive built for this platform, verifies
it against the release SHA-256 `checksums.txt`, and swaps it over the running
executable. It installs the same bits as
[`scripts/install.sh`](../../scripts/install.sh), so a self-update and a fresh
install converge.

| Flag | Description |
|------|-------------|
| `--check` | Report whether an update is available without installing anything. |
| `--force` | Reinstall the latest release even when already up to date. |

```sh
eph update
```

```
Updating eph from v0.3.0 to v0.4.1.
eph updated to v0.4.1.
```

Check without installing:

```sh
eph update --check
```

```
update available: v0.4.1 (current v0.3.0).
Run `eph update` to install it.
```

- The version baked into a **release** binary is a clean `vX.Y.Z` tag, so `eph
  update` compares it against the latest release and reports up to date, an
  available update, or (with `--force`) a reinstall. A **development** build
  (installed with `cargo install --path .` or `make install`, so its version
  carries a `git describe` suffix) has no clean release to compare against and is
  always offered the latest published release.
- The download is checksum-verified against the release `checksums.txt` before a
  single byte is extracted, so a corrupted or tampered archive never reaches your
  binary. This is the same SHA-256 guarantee the install script provides.
- The binary is swapped in place: on Unix an atomic rename replaces it while the
  running process keeps its open image; on Windows, where a running `.exe` cannot
  be overwritten, the old image is moved aside and cleaned up after the process
  exits. Either way, restart any long-running `eph dev` or watch session to pick
  up the new version.
- `EPH_REPO` and `EPH_BASE_URL` override the GitHub repository and download base
  URL, matching the environment variables the install script honors (useful for a
  mirror or an internal fork).
- **Passive out-of-date nag.** Every other command checks, at startup, whether a
  newer release exists and prints a one-line reminder on stderr when one does. The
  check is disconnected from the command you ran: it reads a cached
  latest-release lookup (so it never blocks) and refreshes that cache once a day
  in a detached background process (so there is no network timeout to wait on and
  a failed lookup never affects the command). It stays silent for source builds,
  when stderr is not a terminal (scripts, pipes, CI), and when
  `EPH_NO_UPDATE_CHECK` is set, so it never disturbs `eval "$(eph env)"` or
  machine-readable output. Set `EPH_NO_UPDATE_CHECK=1` to turn it off entirely.

## Commands that do not exist (by design)

The list above is the complete command set. One thing people look for is
deliberately delegated elsewhere:

- **There is no `eph init` or scaffolder.** Create the `.eph` file by hand (see
  [Getting Started](getting-started.md)) and validate it with `eph check`.
