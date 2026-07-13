---
name: using-eph
description: Use when working in a repository that uses eph for ephemeral dev services (a `.eph` file exists, or `eph` is on PATH). Covers workspace detection, strict .eph parsing, auto-assigned ports, up/env/down/clean, roles, hooks, one-off commands, foreground and watch sessions, logs, and stale-workspace pruning.
---

<!-- EPH_SKILL_PROVENANCE -->

# Using eph

`eph` is "dotenv for services": a CLI that starts the dev services a project
needs (Postgres, Redis, MinIO, ...) from a `.eph` file in the project root.
Containers are namespaced by a hash of the workspace path, and direct Docker
ports are assigned randomly. A `run=` app can request the same behavior with
`port=auto`. Bring the services up, read the assigned endpoints from eph, and
tear them down when the task is complete.

## Detect an eph workspace

A workspace is any directory with a `.eph` file (eph searches upward from the
current directory, like git).

```sh
test -f .eph && echo "this project uses eph"
eph check        # validate the file and list services + env vars (no Docker)
eph info         # workspace id, container prefix, state paths (no Docker)
eph status       # running services and assigned host ports
```

`eph check` and `eph info` never touch Docker, so they are always safe to run
first to learn the shape of the workspace.

## The core loop

```sh
eph up                   # start all services: pre-start, pull/build, wait for health, post-start
eval "$(eph env)"        # load resolved connection env vars into your shell
# ... do your work against the running services ...
eph down                 # stop services, keep containers + data for a fast restart
```

- `eph up [SERVICE...]` starts all services, or just the named ones. It is
  idempotent when the effective runtime configuration is unchanged. A running
  service is reused, a stopped container is restarted, and an absent one is
  created. A source, port, environment, volume, health, build, or command change
  reconciles the old backend and creates the requested configuration.
- `eph up` blocks until each service passes its health check, so when it returns
  the services are ready (one exception below for services with no health check).
- An unknown service name is an error.

## Reading the environment (never hardcode ports)

Host ports are random and change every time a container is created, so the only
correct way to reach a service is through `eph env`. Do not read a port out of
the `.eph` file: that is the *container* port, not the published host port.

```sh
eval "$(eph env)"                        # bash / zsh / sh
eph env -f fish | source                 # fish
eph env --format powershell | Out-String | Invoke-Expression   # PowerShell
env_json=$(eph env -f json) || exit $?
printf '%s' "$env_json" | jq -r .DATABASE_URL   # machine-readable
```

`eph env` prints only the top-level `KEY=VALUE` variables from the `.eph` file,
with `${service.port}` style interpolations filled in from **running**
services. When a reference cannot resolve, shell formats explicitly unset the
affected variable and append a failing statement; JSON omits it. Every format
warns on stderr and exits non-zero, so stale shell values and partial machine
configuration cannot pass unnoticed. Run `eph up` first so everything resolves.

## Tearing down

Three teardown levels, from lightest to heaviest:

| Command | Effect |
| --- | --- |
| `eph down` | Stop services; keep containers and volume data for a fast restart. |
| `eph down --rm` (alias `-r`) | Also remove the containers; named-volume data is kept. The next `up` recreates them. |
| `eph clean` | Full reset: remove containers, remove per-workspace named volumes (**deletes that data**), and delete saved state. |

`compose` services are always fully torn down (`--rm` is a no-op for them);
`clean` removes named volumes declared by `image=` and `dockerfile=` services.
Compose services cannot declare `.eph` volumes, and Compose-internal volumes
remain owned by the Compose project.

## Reading the `.eph` file

It is INI-with-`.env`: top-level `KEY=VALUE` lines are shell env vars, and
`[name]` sections are services. Comments must be on their own line (`#`); a `#`
after a value is part of the value, so do not add inline comments.

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
volume=pgdata:/var/lib/postgresql/data
healthcheck=pg_isready -U dev
post-start=npm run db:migrate

[env]
DATABASE_URL=postgres://dev@localhost:${postgres.port}/app
```

- A service names exactly one source: `image=`, `dockerfile=` (+ `context=`),
  `compose=` (+ `expose.<alias>=`), or `run=` (a host process via the platform
  shell). A section with none, or with two, is a parse error.
- `port=` is a *container* port published on a random host port; `${svc.port}`
  in a top-level variable resolves to the assigned host port at `eph env` time.
  `port=`/`port.<name>=` are illegal on `compose=` services (use
  `expose.<name>=` there instead).
- Compose mappings use `expose.<alias>=<compose-service>:<container-port>` and
  resolve as `${svc.port.<alias>}`. The short form
  `expose.<alias>=<container-port>` targets the Compose service named by the
  alias. Missing Compose port output fails startup.
- For a `run=` service (a first-party app eph launches), `port=auto` /
  `port.<name>=auto` make eph allocate a free host port and inject it into the
  process; reference the service's own assigned port as `${svc.port}` in its
  `env.X` (e.g. `env.PORT=${web.port}`). eph keeps the port stable across
  restarts when it remains available. An app that exits during startup is
  retried on a fresh port, for up to four attempts. Read the assigned value via
  `eph env` or `${svc.port}`.
- `env.X=` configures the **service runtime**; it is a different namespace from
  the top-level `DATABASE_URL=` above, which is a **shell** variable emitted by
  `eph env`. For `run=` it is injected into the host process; for Compose it is
  exported to the Compose CLI. **Top-level variables only parse in two
  places**: above the first section, or inside a reserved `[env]` section
  (shown above, right after `postgres`'s properties). `[env]` may repeat, so
  you can group a variable near the service it describes. Sections do **not**
  end at blank lines: a bare `KEY=VALUE` written directly after a service
  section (with no `[env]`) is a parse error. To pass a variable to the service,
  use `env.KEY=` inside its section.
- Environment variable names beginning with `EPH_`, in any letter case, are
  reserved for eph metadata and rejected in every environment scope.
- `volume=name:/path` is a per-workspace named volume; `healthcheck` for an image
  service runs with no shell (whitespace-split, `docker exec`); the lifecycle
  hooks run on the host via `sh -c` with eph's resolved environment injected (see
  below).

## Roles: dependency services vs the app

A `.eph` file can split its services into tiers with a `role=` on each service
and a top-level `roles_order`. The usual split is dependency services (Postgres,
Redis, object storage: things the code talks to, safe to start eagerly) from the
first-party app you are building (start it on demand; it may bind preview ports
or run side effects). Roles let you bring up one tier without the other.

```ini
# dep services come up before the app
roles_order=dep,app

[postgres]
image=postgres:16
role=dep

[web]
run=npm run dev
port=auto
role=app
```

- `roles_order=dep,app` is the linear form: each role depends on the one before
  it. For a graph (a `worker` that needs `dep` but not `app`), use a section
  instead, where each line is `role=dependencies` and a bare `role=` is a root:

  ```ini
  [roles_order]
  dep=
  app=dep
  worker=dep
  ```

- Roles are all-or-nothing: once any service has a `role=`, a `roles_order` is
  required, every service must declare a role listed in it, every listed role
  must have a service, and the graph must be acyclic. `eph check` reports any
  violation. A file with no roles uses implicit ordering (declaration order,
  with `run=` services last).
- Bring-up follows the role graph (dependencies first); teardown reverses it.
- `eph up --role <ROLE>` starts that role **and everything it depends on**, and
  nothing else. Repeatable, and it unions with any positional service names.
  `eph up --role dep` starts just the dependency tier. `eph down --role <ROLE>`
  tears down that role and everything that depends on it.
- In roles mode, a positional service selects only that service from its own
  role, plus whole dependency roles for `up` or whole dependent roles for
  `down`. Peer services in the selected service's own role are not implied.

## Prewarm dependency services at session start

`eph up --role dep` can run from a Claude Code **SessionStart hook** so database
and cache endpoints are available to later tool calls. `eph up` and `eph dev`
reuse those services, and `eph dev` leaves an adopted tier running when it exits.

```sh
#!/usr/bin/env bash
# .claude/hooks/eph-prewarm.sh: prewarm deps and inject their connection env.
# $CLAUDE_ENV_FILE is sourced by Claude Code, so later Bash tool calls in the
# session inherit DATABASE_URL, REDIS_URL, and the rest.
test -f .eph || exit 0
eph up --role dep || exit 0
if [ -n "$CLAUDE_ENV_FILE" ] && resolved_env=$(eph env); then
  printf '%s\n' "$resolved_env" >> "$CLAUDE_ENV_FILE"
fi
```

```json
// .claude/settings.json: run it on session start (project scope: everyone in
// the repo/worktree gets it). Use ~/.claude/settings.json for a personal one.
{
  "hooks": {
    "SessionStart": [
      { "matcher": "startup|resume",
        "hooks": [ { "type": "command", "command": ".claude/hooks/eph-prewarm.sh" } ] }
    ]
  }
}
```

Substitute your own dependency role name for `dep`. The plain command runs
`post-start` hooks, including seeds; add `--skip-hooks` for a bare prewarm. If you
want the tier torn down when a session ends, add a `SessionEnd` hook running
`eph down --role dep`; the default is to leave it warm for the next session.
`eph env` succeeds only when every top-level interpolation resolves. Keep
exported prewarm variables dependent only on services in the selected closure;
put app-only values in the app's `env.*` or load the full environment after the
app starts. The command substitution above discards partial output.

## Lifecycle hooks see eph's environment

Four hooks bracket a service, in order: `pre-start` (before it is created),
`post-start` (after it is healthy), `pre-stop` (before it stops), `post-stop`
(after it has stopped). All run with the same variables `eph env` emits already
in their environment, so a database migration or codegen step just works:

```ini
[postgres]
image=postgres:16-alpine
port=5432

[api]
run=./bin/server
# runs before the server boots
pre-start=go generate ./...
# DATABASE_URL is already set
post-start=psql "$DATABASE_URL" -f schema.sql
pre-stop=./scripts/backup.sh
# cleanup eph cannot do itself
post-stop=rm -rf .cache/scratch

[env]
DATABASE_URL=postgres://dev@localhost:${postgres.port}/app
```

Each hook receives, layered in this order (later wins):

1. the resolved top-level `.eph` variables (what `eph env` prints, except that
   during `eph up` a hook also sees the reserved ports of `run=` services
   being started, which `eph env` cannot show until they are up),
2. `EPH_*` metadata: `EPH_WORKSPACE_ID`, `EPH_WORKSPACE_ROOT`,
   `EPH_CONTAINER_PREFIX`, and per service `EPH_<SERVICE>_HOST`,
   `EPH_<SERVICE>_PORT`, `EPH_<SERVICE>_PORT_<NAME>` (for named ports),
   `EPH_<SERVICE>_CONTAINER` (service names upper-cased, `-` -> `_`),
3. the owning service's own `env.X=` values.

`pre-start` runs just before its own service is created. It sees any service
already up (backing services start before `run=` apps), plus the reserved port
of every `run=` service the same invocation is starting, its own included: eph
decides those ports before any hook runs, so a top-level variable like
`APP_URL=http://localhost:${api.port}` already resolves in the hook's
environment. Only a container port Docker has not assigned yet (an `image=`
service later in the start order) is unavailable to it. Use it for prep the
service depends on, like codegen.
`post-start` hooks run only after **every** service in the `up` is healthy, so a
hook may reference any other service's port (`${redis.port}` resolves even if
redis started after the service whose hook needs it). `post-stop` runs after its
service has stopped and sees the same pre-teardown environment as `pre-stop`.

`pre-start` and `post-start` run on **every** `eph up` (fresh create *or*
restart); a failing `pre-start` aborts the `up` before its service starts and a
failing `post-start` aborts the `up`. A failing `pre-stop` aborts the `down` /
`clean` and leaves the service running so you can fix and retry; a failing
`post-stop` aborts the rest of teardown, but its service is already stopped.
Write hooks to be idempotent (a migration that no-ops when already applied, an
`INSERT ... ON CONFLICT` seed). For one-off, non-idempotent work, use `eph run`
instead.

Pass `--skip-hooks` to skip hooks for one invocation: `eph up --skip-hooks`
starts services without `pre-start`/`post-start`; `eph down --skip-hooks` /
`eph clean --skip-hooks` tear down without `pre-stop`/`post-stop` (the escape
hatch when a broken hook is wedging teardown).

## Running one-off commands with the environment: `eph run`

`eph run <cmd>...` runs a command in the workspace root with the resolved
environment (the `eph env` variables plus the `EPH_*` metadata) already set, so
you do not have to `eval "$(eph env)"` first:

```sh
eph run psql "$DATABASE_URL"        # NOTE: $DATABASE_URL is expanded by YOUR shell
eph run ./scripts/seed.sh           # the script sees DATABASE_URL, EPH_* itself
eph run sh -c 'psql "$DATABASE_URL" < dump.sql'   # use sh -c for shell features
```

The command is executed directly, not through a shell, so eph does not expand
`$VAR` in the arguments; wrap it in `sh -c '...'` when you need shell expansion
of eph's injected variables, piping, or globbing. `eph run` exits with the
command's native exit status and refuses to launch when any top-level reference
is unresolved. Use it for repeatable operations (seeding, resets,
ad-hoc queries): unlike `post-start`, it runs every time you invoke it.

Every token after `run` belongs to the command, flag-shaped or not: `eph run
-v ./script.sh`, `eph run -h`, and `eph run --foo` all pass `-v`/`-h`/`--foo`
straight through as the command's own argument, with no `--` separator needed.
Only a flag placed *before* `run` (`eph -v run ...`) is still eph's own.

## Claude Desktop preview servers: `eph dev`

`eph dev` runs the whole stack in the foreground for a Claude Desktop preview
server (`.claude/launch.json`), which launches one command and watches its port
but has no setup or teardown hook. `eph dev` fills both: it brings up the
backing services (each one's `pre-start` running right before it starts, same
interleaving as `eph up`), runs the foregrounded app's own `pre-start`, starts
a `run=` app with eph's own stdin, stdout, and stderr wired through to it, runs
every service's `post-start` (seeding) once everything is up, and on stop tears
the stack down: `eph down` by default, or `eph clean` with `--clean` (each
running `pre-stop` then `post-stop`). Pass `--skip-hooks` to skip all four hook
phases for the whole session, matching `eph up --skip-hooks` /
`eph down --skip-hooks`.

**Running `eph dev` yourself? Launch it in the background.** `eph dev` foregrounds
the app and does not return until the app exits, so running it as an ordinary
(blocking) command hangs you until the tool call times out. When you invoke it
directly (not through the preview server, which already backgrounds it), start it
detached, then poll for readiness and keep working in the same shell:

```sh
# Claude Code: set run_in_background: true on the Bash call that starts it.
# Any shell: background it yourself and capture its output to a log.
eph dev > eph-dev.log 2>&1 &
dev_pid=$!
until grep -q "^Serving '" eph-dev.log; do
  kill -0 "$dev_pid" || exit 1
  sleep 1
done
# Run tests or requests against the live stack.
```

Add repeatable workspace-relative globs when the session should restart on
edits: `eph dev --watch "**/*.rs" --watch "*.toml"`.

Stop it by ending that background process: a normal stop signal lets `eph dev`
tear the stack down, while a hard kill leaves the services up (recover them with
`eph down`). The same rule applies to any eph command that stays in the
foreground, most notably `eph logs -f`. Commands that return on their own
(`eph up`, `eph env`, `eph check`, `eph status`) never need backgrounding.

```jsonc
// .claude/launch.json -- point the preview server at eph dev
{
  "version": "0.0.1",
  "configurations": [
    { "name": "web", "runtimeExecutable": "eph", "runtimeArgs": ["dev"], "port": 3000, "autoPort": true }
  ]
}
```

- Model the app as a `run=` service with `port=auto`. The app runs on its own
  internal port; `eph dev` opens the `$PORT` the preview server injects (its
  `autoPort`) as a forwarding gate to the app, but only *after* `post-start`
  hooks finish. Since the preview watches `$PORT`, it does not see the app as
  ready until seeding is done, not the instant the server can answer a health
  check. Do not also give the app a fixed port.
- With no SERVICE the sole `run=` service is foregrounded; `eph dev <service>`
  picks one when the `.eph` defines several.
- `--watch <glob>` is repeatable. It restarts the services `eph dev` started
  when a matching workspace-relative path changes. A restart runs the full
  stop/start hook sequence, keeps named volumes, ignores `.git`, and leaves
  adopted prewarmed services running.
- In watch mode an app crash leaves the session waiting for a matching change;
  without watch mode the command exits non-zero and leaves backing services up
  for inspection.
- Teardown defaults to `eph down` (keeps data for a fast relaunch, since Claude
  restarts the server during a session). Use `eph dev --clean` (`runtimeArgs:
  ["dev", "--clean"]`) for a pristine reset on every launch.
- `eph dev` tears down only the services it actually started. Any that were
  already running when it launched (a dependency tier a SessionStart hook
  prewarmed) are left up, so the deps stay hot across `eph dev` runs. `--clean`
  overrides this and bulldozes everything.
- A hard kill (not a normal stop) skips teardown and leaves services up,
  recoverable with `eph down`. If the app crashes on its own, `eph dev` leaves
  services up for inspection (`eph logs <service>`) and exits non-zero.
- `eph` must be on the app's PATH; the desktop app may not inherit your shell
  PATH, so use an absolute path in `runtimeExecutable` if it cannot find `eph`.

## Inspecting logs: `eph logs`

```sh
eph logs                 # all services interleaved, each line tagged [name]
eph logs -f              # follow all services at once; Ctrl-C to stop
eph logs <service>       # one service, raw (untagged)
eph logs -f <service>    # follow one service
eph logs -n 50 <service> # last 50 lines
```

Works for every service type: `run=` services read from a captured log file,
while `image=` / `dockerfile=` / `compose=` services proxy `docker logs` /
`docker compose logs`. With no service, every service is streamed concurrently
and interleaved (compose-style), each line prefixed by a color-coded `[name]`
tag; a single `eph logs <service>` is untagged and pipe-friendly. Logs show even
for a stopped service, so a `run=` service that died on startup still leaves a
trace; check `eph logs <service>` when a service is missing from `eph status`.
The `-f` (follow) forms block until interrupted, so an agent must run them in the
background (Claude Code: `run_in_background: true`); the non-follow forms print and
return, so they run as normal commands.

## Behaviors that matter

- **Foreground commands must be backgrounded by an agent.** `eph dev` (and
  `eph logs -f`) stay in the foreground and never return on their own, so launch
  them detached (Claude Code: `run_in_background: true`; any shell: append `&`) or
  they hang your tool call until it times out. `eph up`, `eph env`, `eph check`,
  and `eph status` return on their own, so they run as normal blocking commands.
- **Ports are random and change per create.** Never hardcode a host port; always
  go through `eph env`.
- **Idempotent up.** A running service is reused and a stopped one restarted, but
  only when its canonical runtime fingerprint still matches. Effective config
  drift removes the old backend and recreates it; Dockerfile sources build on
  every `up` through Docker's cache so context changes are included. Reused
  services rerun declared health checks. Failed starts are removed before eph
  returns, so retries cannot adopt an unhealthy leftover.
  `pre-start` and `post-start` hooks run on **every** `eph up` regardless. Write
  them to be idempotent (migrations that no-op when applied, codegen that
  overwrites in place), or move one-off work to `eph run`. A failing `pre-start`
  aborts the `up` before its service starts; a failing `post-start` aborts the
  `up`.
- **Image health checks have no shell**: one whitespace-split command, no pipes,
  `&&`, `$VAR`, or quoted spaces, and the binary must exist in the image.
- **A service with no health check** is given a fixed short wait, so it may need a
  moment to accept connections after `eph up` returns; retry your first connect.
- **Isolation by path**: two checkouts are different containers, volumes, and
  ports. There is no `eph init` (author `.eph` by hand).
- **Process teardown requires identity.** eph captures a `run=` process's
  identity at launch and verifies it before signaling the recorded PID. A state
  entry without identity requires manual process inspection and cleanup.
- **Deleted workspaces are global state.** `eph system prune --dry-run` lists
  resources for missing or empty workspace paths. A real prune confirms before
  removal. `--force-non-empty` also selects paths that still contain files, and
  live resources require `--force-live` regardless of how the path was selected.
- **Execution fails closed on unresolved references.** Hooks, service startup,
  health checks, and `eph run` stop before launching a child with a raw eph
  placeholder.
- **Output is on stdout; logs go to stderr.** `eph env` output is clean for
  `eval` and piping. Add `-v` / `--verbose` for debug logging on stderr.

## Safe defaults for automation

1. Validate before acting: `eph check`.
2. Start and load in the **same** step so the env is fresh:
   `eph up && eval "$(eph env)" && <your command>`.
3. Tear down in an always-run step (even on failure): `eph clean`.
4. Treat `.eph` as possibly holding dev credentials: do not print it to logs or
   commit one with real secrets.
