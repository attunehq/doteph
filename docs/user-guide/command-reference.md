---
title: "Command Reference"
summary: "Every command, every flag, and what each one prints."
order: 9
---

# Command Reference

Every `eph` command, its flags, and what it prints. Most commands operate on
the workspace found by searching up from the current directory for a `.eph`
file; `eph system prune` is the exception and works globally.

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

Start services. With no arguments, starts every service in the `.eph` file.
Without a role graph, names select exactly those services. With roles, a name
also pulls in every service from the roles it depends on, but not peer services
from the named service's own role. An unknown service name is an error.

| Flag | Description |
|------|-------------|
| `--role ROLE` | Bring up this role and everything it depends on. Repeatable; combines with SERVICE names (the union starts). Requires a `roles_order`. |
| `--skip-hooks` | Bring services up healthy but run no `pre-start` or `post-start` hooks. |

```sh
eph up                 # all services
eph up postgres redis  # just these two
eph up --role dep      # the dep tier and its dependencies (prewarm)
eph up --role app      # the app plus every role it depends on
eph up --skip-hooks    # skip codegen/migrations this once
```

Behavior:

- **Idempotent with reconciliation.** A matching running service is reused and
  a matching stopped resource is restarted. Effective source, image, port,
  resolved environment, volume, health, build, or command drift removes the old
  backend and creates the requested one. Reused services rerun declared health
  checks. See [Core Concepts](concepts.md#the-service-lifecycle).
- **Hooks bracket it.** Each service's `pre-start` hooks run just before it is
  created; once every targeted service is healthy, all `post-start` hooks run
  in a second phase. Both run on **every** `eph up`, with eph's resolved
  environment injected, and a failure aborts the command. Full rules in
  [The `.eph` File](eph-file.md#lifecycle-hooks).
- **`--role` takes the dependency closure**: the role plus every role it
  transitively depends on, since a role cannot run without what is below it.
  With `roles_order=dep,app`, `--role app` starts both tiers and `--role dep`
  starts only `dep`. Using `--role` without a `roles_order` in the file is an
  error. See [Roles and ordering](eph-file.md#roles-and-ordering).
- **A positional service also respects the graph.** With a `web` service in
  `app`, `eph up web` starts `web` plus every service in the roles below
  `app`. Other `app` services remain stopped. Use `--role app` to select the
  whole role.
- Prints each started service and its assigned host port.
- `eph up`, `eph down`, and `eph clean` on the same workspace serialize
  against each other; a second command started while one is still running
  waits and prints a notice rather than racing it. See
  [Persisted state](concepts.md#persisted-state).
- After a successful `up`, a filesystem-only scan checks whether any *other*
  workspace's recorded path has been deleted (a removed worktree or clone),
  and prints a one-line note on stderr pointing at
  [`eph system prune`](#eph-system-prune---dry-run---compatibility-v042---force-non-empty---force-live--y---yes)
  when it finds one. It never touches Docker, never fails the `up` itself, and
  never counts the current workspace.

## `eph down [--rm] [SERVICE...]`

Stop services. With no arguments, stops all. Without a role graph, names stop
exactly those services. With roles, a name also stops every service in
roles that depend on its role, but not peer services from the named service's
own role.

| Flag | Description |
|------|-------------|
| `--role ROLE` | Stop this role and everything that depends on it, in reverse start order. Repeatable; combines with SERVICE names. Requires a `roles_order`. |
| `-r`, `--rm` | Also remove the stopped containers. |
| `--skip-hooks` | Stop without running `pre-stop` or `post-stop` hooks. |

```sh
eph down               # stop all, keep containers
eph down --rm          # stop all and remove containers
eph down postgres      # stop just postgres
eph down --role dep    # stop the dep tier and everything above it
eph down --skip-hooks  # bypass a broken teardown hook
```

Behavior:

- Without `--rm`, containers and their data remain for a fast restart. With
  `--rm`, containers are removed (named-volume data is kept) and the next
  `eph up` creates fresh ones.
- Each service runs `pre-stop` before stopping and `post-stop` after. A
  failing `pre-stop` aborts the `down` and leaves the service running; a
  failing `post-stop` aborts the rest of the teardown. See
  [The `.eph` File](eph-file.md#teardown-hooks-pre-stop-and-post-stop).
- **`--role` takes the dependent closure**: the role plus every role that
  transitively depends on it, because a dependency cannot go away while the
  roles that need it are up. With `roles_order=dep,app`,
  `eph down --role dep` stops both `app` and `dep`. Without a role graph,
  `eph down` stops exactly what it targets.
- **A positional service also protects dependents.** If `db` belongs to `dep`,
  `eph down db` stops `db` and every service in roles above `dep`. Another
  service in `dep` remains running. Use `--role dep` to stop the whole role.
- Two per-source exceptions: **compose** services are always torn down with
  `docker compose down`, so `--rm` makes no difference for them, and **run**
  services are always killed (there is no container to keep). A targeted
  `eph down <service>` persists the updated state immediately. A failed
  `docker compose down` (for example, a missing `docker compose` plugin) is a
  real error and aborts the rest of the teardown, rather than
  being silently swallowed.
- **A bare `eph down` (no service names) also tears down recorded state that
  no longer matches the `.eph` file.** Teardown works from what
  `state.json` says eph actually started, not just the sections currently
  declared: a service you renamed or deleted from the file is still stopped
  and its container removed if state remembers starting it. A targeted
  `eph down <service>` only accepts names that still exist in the file, so it
  cannot reach a renamed entry by its old name; use the bare form to sweep
  those up.

## `eph clean`

Full reset for the workspace. Stops and removes every service's container
(or Compose project, or process), removes every **per-workspace named
volume**, and deletes the persisted state directory.

| Flag | Description |
|------|-------------|
| `--skip-hooks` | Tear everything down without running `pre-stop` or `post-stop` hooks. |

```sh
eph clean
eph clean --skip-hooks   # reset even if a teardown hook is broken
```

```
Workspace cleaned:
  Services stopped and removed: 3
  Named volumes removed: 2
  Persisted state: removed
```

The counts are **measured**, not the number of services declared in the
`.eph` file: they count only what was actually stopped or removed. A
workspace whose services never started reports zeros across the board.

> This **deletes the data** in named volumes. Bind mounts (host paths) are
> never touched, and volumes internal to a Compose file are left to
> `docker compose`.

Like `eph down`, `clean` runs each service's teardown hooks, and a failing
hook aborts the reset; `--skip-hooks` is the escape hatch.

Behavior beyond the declared services:

- **Renamed or deleted sections are still cleaned up.** Like a bare
  `eph down`, `clean` tears down from `state.json`'s record of what eph
  actually started, so a service you renamed or removed from the `.eph` file
  is still stopped, its container removed, and its state entry dropped.
- **A final sweep catches anything state does not know about either.**
  `clean` also removes any leftover Docker container or volume still carrying
  the workspace's `eph-<short_id>-` name prefix, for a service renamed before
  its state was ever recorded, or a container left behind by a crash before
  `eph up` finished writing state. This is the one place `clean` looks past
  both the `.eph` file and `state.json`, because `clean` promises a full
  reset.

## `eph system prune [--dry-run] [--compatibility-v042] [--force-non-empty] [--force-live] [-y] [--yes]`

Cross-workspace prune for resources left behind after workspace directories
are deleted (finished worktrees, removed clones). It scans the eph state root
(the platform default, or `EPH_STATE_ROOT` when set; see
[Persisted state](concepts.md#persisted-state)), reads each workspace's
recorded path, and removes resources for workspaces whose path is gone or is
an empty directory.

| Flag | Description |
|------|-------------|
| `--dry-run` | Print what would be removed without deleting anything, and without prompting. |
| `--compatibility-v042` | Also prune 8-character state directories that have no workspace metadata. |
| `--force-non-empty` | Also prune workspaces whose recorded path still exists and contains files. |
| `--force-live` | Remove a stale workspace's resources even if it still has running containers or a live `run=` process. |
| `-y`, `--yes` | Skip the removal confirmation prompt. |

```sh
eph system prune
eph system prune --dry-run
eph system prune --yes
eph system prune --compatibility-v042
eph system prune --force-non-empty --dry-run
eph system prune --force-non-empty --yes
eph system prune --force-live --yes
```

```text
System prune dry run:
  a1b2c3d4e5f60718 (missing workspace) - C:\Users\me\.codex\worktrees\1234\app
    containers: 2, volumes: 1, networks: 1, images: 1, run processes: 0, state dirs: 1

Totals:
  Containers: 2
  Volumes: 1
  Networks: 1
  Images: 1
  Verified run= processes: 0
  State directories: 1

Remove these resources? [y/N] y
System prune complete:
  a1b2c3d4e5f60718 (missing workspace) - C:\Users\me\.codex\worktrees\1234\app
    containers: 2, volumes: 1, networks: 1, images: 1, run processes: 0, state dirs: 1

Totals:
  Containers: 2
  Volumes: 1
  Networks: 1
  Images: 1
  Verified run= processes: 0
  State directories: 1
```

Behavior:

- By default, a recorded workspace path is eligible only when it is missing,
  empty, or no longer a directory. `--force-non-empty` also makes existing
  non-empty directories eligible. This is a global override, so preview it
  with `--dry-run` before removal.
- Progress is logged to stderr while prune acquires its lock, inventories
  Docker, scans state directories, and removes resources. The final report
  remains on stdout for callers that capture or pipe it. Prune lists each
  Docker resource type once per pass and matches the resulting snapshot to
  workspace namespaces in memory, so large state roots do not cause repeated
  Docker API calls.
- A workspace's recorded path only decides whether it is *stale*, not whether
  something is still running against it: before removing anything for a
  stale workspace, prune checks that workspace's actual Docker containers and
  `run=` processes for signs of life. This matters because a workspace that
  was merely moved or renamed while its services keep running looks exactly
  like a deleted one from the recorded path alone; without the check, prune
  would force-kill those live containers and delete their volume data with no
  warning. If any container is running, or a recorded `run=` process is alive
  under the identity eph captured at launch, the workspace is reported under
  "Skipped" instead ("stop them or re-run with --force-live") and left
  untouched. `--force-live` authorizes removing it anyway. The two force flags
  are independent: a non-empty workspace with live resources requires both
  `--force-non-empty` and `--force-live`. `--dry-run` applies the same checks,
  so its preview always matches what a real run would do.
- Unless `--dry-run`, prune prints what it is about to remove and then asks
  `Remove these resources? [y/N]` before deleting anything, the same way
  `docker system prune` does. Anything other than `y` or `yes` (a bare Enter
  included) aborts with nothing removed, and the command still exits
  successfully. Pass `-y`/`--yes` to skip the prompt; it is required when
  stdin is not a terminal (a script or CI job, for instance), where prune
  errors instead of hanging or silently proceeding. No prompt appears when
  there is nothing to remove.
- Docker resources are removed by eph's workspace namespace
  (`eph-<short_id>-...`), so containers, built images, named volumes, Compose
  containers, and Compose networks can all be pruned even when the original
  `.eph` or compose file is gone. The workspace state directory is deleted
  last.
- For `run=` services, only a PID whose current process identity matches the
  identity eph recorded at launch is killed. A process entry without identity,
  and a mismatched PID that may have been reused, are skipped with a warning. A
  command that detached grandchildren outside the shell tree eph launched
  leaves processes prune cannot discover; stop those manually.
- An 8-character state directory without `workspace.json` is skipped by
  default. `--compatibility-v042` prunes that directory by `short_id` namespace
  alone. Preview this path with `--dry-run --compatibility-v042`.
- Prune holds an OS-level lock file (`prune.lock` in the state root) for its
  whole run, so two prunes never operate at once. The lock is released the
  instant the holding process exits, crash included, so a second prune
  started while one is already running fails immediately with a clear error
  instead of racing it or wedging on a leftover lock file.
- A real prune also takes each candidate's workspace lifecycle lock before it
  inventories Docker. If `up`, `down`, `clean`, or foreground `dev` startup is
  already changing that workspace, prune waits for it and then inventories the
  resulting resources. This keeps the live-resource guard accurate for
  existing non-empty workspaces.

## `eph dev [SERVICE] [--clean] [--watch GLOB]... [--skip-hooks]`

Run the whole dev stack as one foreground process: bring services up, run
`post-start` hooks (seeding), foreground a `run=` service with eph's stdin,
stdout, and stderr wired through, and tear down what it started when stopped.
The full walkthrough, including preview servers and the `$PORT` readiness
gate, is in [Running Your App](run-your-app.md#eph-dev-the-foreground-loop).

| Flag | Description |
|------|-------------|
| `--clean` | On the final stop, tear the whole workspace down with `eph clean` (drops named volumes and their data) instead of the default `eph down`. |
| `--watch GLOB` | Restart the stack when a file matching GLOB changes. Repeatable; globs are relative to the workspace root with gitignore-style separators. |
| `--skip-hooks` | Bring the stack up and tear it down without running any lifecycle hooks, matching `eph up --skip-hooks` / `eph down --skip-hooks` together. |

```sh
eph dev              # foreground the sole run= service; eph down on stop
eph dev web          # foreground a specific run= service by name
eph dev --clean      # full reset on the final stop
eph dev --skip-hooks # bring up and tear down with no lifecycle hooks
eph dev --watch "**/*.rs" --watch "*.toml"   # restart on source changes
```

Behavior:

- With no `SERVICE`, the sole `run=` service is foregrounded; name one when
  the file defines several. A `.eph` with no `run=` service is an error.
- **Hooks run in exactly the order `eph up` uses.** Each backing service's
  `pre-start` runs immediately before that service starts (so it can
  reference services already up); the foregrounded app's own `pre-start` runs
  immediately before it starts, seeing every backing service's assigned port.
  `post-start` hooks for every service, foreground app included, run together
  in a second phase once everything is up, so a `post-start` hook may
  reference any service's port. `--skip-hooks` skips all four hook phases for
  both bring-up and teardown.
- On stop (the preview server's stop, or Ctrl-C), only the services `eph dev`
  started itself are torn down; services that were already running when it
  began (a prewarmed tier) are left up. A hard kill (`SIGKILL`) cannot run
  teardown; recover with `eph down`.
- If the app exits on its own, `eph dev` exits non-zero and leaves the backing
  services up, except in watch mode, where it reports the exit and waits for
  the next file change to restart.
- When the environment sets `$PORT` (a preview server's `autoPort`), `eph dev`
  opens that port as a forwarding gate to the app only after `post-start`
  hooks finish, so a watching preview cannot go live before seeding is done.
- A `--watch` restart is a full down and up (all hooks fire, volumes always
  kept); changes are debounced, and churn under `.git` is ignored.

## `eph status`

Show the workspace and which services are running. Reconciles saved state
against the live Docker daemon and tracked PIDs, so manually removed
containers drop out.

```sh
eph status
```

```
Workspace: /home/you/projects/myapp
ID: a1b2c3d4e5f60718

Running services:
  postgres -> localhost:54321
  redis -> localhost:54322

Stopped services:
  minio
```

The `ID:` shown is the short ID; `eph info` also shows the full SHA-256
workspace ID. All four service types are reconciled: `image` and `dockerfile`
by container name, `run` by tracked process, and `compose` by the Compose
project's `com.docker.compose.project` label.

## `eph env [-f FORMAT]`

Print the top-level environment variables from the `.eph` file, with
`${service.property}` references resolved against **running** services. Built
for shell `eval`; see [Shell Integration](shell-integration.md).

| Flag | Values | Default |
|------|--------|---------|
| `-f`, `--format` | `export`, `fish`, `powershell`, `json` | `export` |

```sh
eval "$(eph env)"                                  # bash / zsh / sh
eph env -f fish | source                           # fish
eph env --format powershell | Out-String | Invoke-Expression   # PowerShell
env_json="$(eph env -f json)" && jq -r .DATABASE_URL <<<"$env_json"
```

- Only top-level variables are printed; service `env.*` values are not.
- If a value still contains an unresolved `${service.property}`, shell formats
  unset that variable and then execute a failing statement. JSON omits the
  variable. `eph env` reports the missing reference on stderr and exits
  nonzero in every format. This clears stale values while making the incomplete
  environment observable to both shell evaluation and scripts.
- All running services resolve, including `compose` services (their
  `expose.<name>` ports resolve as `${service.port.<name>}`).
- `--format json` keys appear in the `.eph` file's declaration order.
- An unknown format is an error
  (`unknown format: ..., use: export, fish, powershell, json`).

## `eph run <CMD>...`

Run a command in the workspace root with eph's resolved environment already
set: the same variables `eph env` prints, plus the `EPH_*` metadata (see
[Hook environment](eph-file.md#hook-environment)).

```sh
eph run ./scripts/seed.sh            # the script sees DATABASE_URL, EPH_*, ...
eph run psql "$DATABASE_URL"         # $DATABASE_URL expanded by YOUR shell
eph run sh -c 'psql "$DATABASE_URL" < dump.sql'   # sh -c for shell features
```

- The command is executed directly, **not** through a shell, so eph does not
  expand `$VAR`, globs, or pipes in the arguments. Wrap the command in
  `sh -c '...'` when you need shell features driven by eph's injected
  variables.
- The command is not started if any top-level variable still references a
  stopped service. Run `eph up` first.
- Exits with the command's native process status. Windows exit codes are not
  narrowed to eight bits; Unix signal exits use the shell convention
  `128 + signal`.
- Unlike a `post-start` hook, `eph run` executes only when you invoke it. Use
  it for repeatable operations: seeding, resets, ad-hoc queries.
- **Every token after `run` belongs to the command**, including ones shaped
  like eph's own flags: `eph run -v ./script.sh`, `eph run -h`, and
  `eph run --foo` all pass `-v`/`-h`/`--foo` straight through as the command's
  own arguments, with no `--` separator needed. A flag placed *before* `run`
  (`eph -v run ...`) is still eph's own: only the tokens before `run` on the
  command line are eph's flags (`-v`/`--verbose`).

## `eph logs [SERVICE] [-f] [-n N]`

Show service logs across every service type from one command: `run` services
read from the log file eph captures their output to, `image` and `dockerfile`
services proxy `docker logs`, and `compose` services proxy
`docker compose logs`.

| Flag | Description |
|------|-------------|
| `-f`, `--follow` | Stream new output as it arrives (like `tail -f`); Ctrl-C to stop. Works with or without a `SERVICE`. |
| `-n`, `--tail N` | Show only the last `N` lines before printing or streaming. |

```sh
eph logs                      # every service interleaved, each line tagged [name]
eph logs -f                   # follow all services at once
eph logs worker               # just the worker service (raw, untagged)
eph logs -f worker            # follow worker
eph logs -n 50 postgres       # last 50 lines
```

- Logs are shown **even for a stopped service**, so a `run` service that died
  on startup still leaves an inspectable trace (its output is captured to
  `<state-dir>/logs/<service>.log`; `eph info` shows the state directory).
- A `run` service's log file is truncated on each fresh start, so it reflects
  the current run.
- With no `SERVICE`, every service streams concurrently, interleaved in
  arrival order, each line prefixed with a right-aligned, color-coded `[name]`
  tag. Lines are emitted whole, never split mid-line. Colors go only to a
  terminal and are suppressed when `NO_COLOR` is set or output is piped.
- `eph clean` removes the captured log files with the rest of the state.

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
Workspace ID: a1b2c3d4e5f60718293a...  (full SHA-256)
Short ID: a1b2c3d4e5f60718
Container prefix: eph-a1b2c3d4e5f60718
.eph file: /home/you/projects/myapp/.eph
State directory: /home/you/.local/share/eph/a1b2c3d4e5f60718
```

Use the container prefix and short ID to find this workspace's resources with
the `docker` CLI. The state directory's parent (the `eph` above the short ID)
honors an absolute `EPH_STATE_ROOT`; relative overrides are rejected. See
[Persisted state](concepts.md#persisted-state).

## `eph skills install [--dir DIR] [--force]`

Install the agent skills bundled into the `eph` binary into the repository, so
a coding agent working in the checkout discovers how to use `eph` (drive
`eph up`, load `eph env`, tear down). The skills are embedded in the binary,
so this works offline.

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

- The target is the **git repository root** containing the current directory,
  so the skills land at the top of the repo regardless of where you run it. It
  falls back to the current directory outside a git repo, printing a warning
  on stderr that names the directory it installed into, since that fallback is
  easy to trigger by accident (running from the wrong place) and easy to miss
  otherwise.
- A `--dir` value must be a plain relative path: an absolute path, a `..`
  component, or a Windows drive-relative path like `C:foo` (no separator after
  the colon) are all rejected, naming the offending directory. `C:foo` is
  rejected alongside the others because, despite not being absolute, joining
  it onto the repo root replaces the root outright instead of nesting inside
  it, the same escape an absolute path or a `..` gets.
- A file that already matches what the binary would write is reported as
  `unchanged`. One that differs is left untouched and reported as `skipped`
  unless you pass `--force`, so a local edit is never clobbered silently.
- Commit the written files. `eph skills check` reports whether they match the
  installed binary; `eph skills install --force` replaces drifted copies.

## `eph skills check [--dir DIR]`

Verify the installed skills match the binary's embedded source, without
changing anything. Prints one line per file and **exits non-zero** if any is
missing or has drifted, so CI can run it as a drift guard.

```sh
eph skills check
```

```
  up to date: .claude/skills/using-eph/SKILL.md
  up to date: .agents/skills/using-eph/SKILL.md
```

The rendered skill contains no build version, so matching text is byte-stable.
The check fails only when a file is missing or its content differs from the
binary's embedded source.

## `eph skills list`

List the skills bundled into this `eph` binary, with the version they ship in.

```sh
eph skills list
```

## `eph update [--check] [--force]`

Update `eph` to the latest GitHub release, replacing the running binary in
place. The updater is native (no dependency on `curl` or a shell): it resolves
the latest published release, downloads the archive built for this platform,
verifies it against the release's SHA-256 `checksums.txt`, and swaps it over
the running executable. It installs the same bits as
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

Behavior:

- A **release** binary carries a clean `vX.Y.Z` version, so `eph update`
  compares it against the latest release and reports up to date, an available
  update, or (with `--force`) a reinstall. A **development** build (installed
  with `cargo install --path .` or `make install`, whose version carries a
  `git describe` suffix) has no clean release to compare against and is always
  offered the latest published release.
- The download is checksum-verified before a single byte is extracted, the
  same SHA-256 guarantee the install script provides.
- The swap is platform-correct: on Unix an atomic rename replaces the binary
  while the running process keeps its open image; on Windows, where a running
  `.exe` cannot be overwritten, the old image is moved aside and cleaned up
  after the process exits. Either way, restart any long-running `eph dev` or
  watch session to pick up the new version.
- `EPH_REPO` and `EPH_BASE_URL` override the GitHub repository and download
  base URL, matching the install scripts' environment variables (see
  [Getting Started](getting-started.md#install); useful for a mirror or an
  internal fork).
- **Passive out-of-date nag.** Every other command checks at startup whether a
  newer release exists and prints a one-line reminder on stderr when one does.
  The check reads a cached latest-release lookup (it never blocks the command)
  and refreshes that cache at most once a day in a detached background
  process, so a failed lookup never affects the command you ran. The cache is
  namespaced per `EPH_REPO`, so pointing `EPH_REPO` at a fork to test a build
  cannot poison (or borrow) the default repo's cached nag. It stays silent for
  source builds, when stderr is not a terminal (scripts, pipes, CI), and when
  `EPH_NO_UPDATE_CHECK` is set, so it never disturbs `eval "$(eph env)"` or
  machine-readable output.

## Commands that do not exist (by design)

The list above is the complete command set. One thing people look for is
deliberately absent:

- **There is no `eph init` or scaffolder.** Create the `.eph` file by hand
  (see [Getting Started](getting-started.md)) and validate it with
  `eph check`.
