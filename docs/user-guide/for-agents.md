---
title: "For Agents and Scripts"
summary: "A terse quick reference for AI coding agents and automation."
order: 10
---

# For Agents and Scripts

A terse, scannable reference for AI coding agents and automation working in a
repo that uses `eph`. Everything here is also explained, with rationale, in
the rest of the [user guide](README.md). If you are an agent, you can act
from this page alone.

## What eph is

A CLI that starts per-workspace dev services (Postgres, Redis, and so on)
from a `.eph` file in the project root. Containers are namespaced by a hash of
the workspace path; host ports are auto-assigned. It is `.env` for services.

## Detect and inspect

```sh
test -f .eph && echo "this project uses eph"   # a workspace has a .eph file
eph check        # validate the file, list services + env vars (no Docker)
eph info         # workspace id, container prefix, paths (no Docker)
eph status       # what is currently running, with ports
```

## Core loop

```sh
eph up                       # start all services (idempotent, waits for health)
eph env -f json              # machine-readable resolved env vars (stdout)
eval "$(eph env)"            # load resolved env into the shell
eph down                     # stop (keep containers + data)
eph down --rm                # stop and remove containers (keep volume data)
eph clean                    # remove containers + named volumes (DATA LOSS) + state
```

Prefer `eph env -f json` for parsing:

```sh
env_json=$(eph env -f json) || exit $?
DATABASE_URL=$(printf '%s' "$env_json" | jq -r .DATABASE_URL)
```

## Prewarm dependency services on session start

If the `.eph` file defines roles (a `roles_order` plus a `role=` on every
service), you can bring up just the **dependency tier** without starting the
first-party app. This is the recommended agent integration: a Claude Code
**SessionStart hook** that prewarms the databases and caches, injects their
connection env, and leaves the app alone (starting it could bind preview
ports or trigger side effects the agent did not ask for).

The hook runs `eph up --role dep` (substitute the file's actual dependency
role name), then appends `eph env` to the file named by `$CLAUDE_ENV_FILE`,
which Claude Code sources so later Bash tool calls inherit `DATABASE_URL` and
friends:

```sh
#!/usr/bin/env bash
# SessionStart hook: prewarm dependency services and inject their env.
eph up --role dep >/dev/null 2>&1 || exit 0
[ -n "$CLAUDE_ENV_FILE" ] && eph env >> "$CLAUDE_ENV_FILE"
```

Wire it in `.claude/settings.json` (project scope, so everyone opening the
repo or a worktree gets it):

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume",
        "hooks": [ { "type": "command", "command": ".claude/hooks/eph-prewarm.sh" } ]
      }
    ]
  }
}
```

Notes:

- `eph up` is idempotent, so a later `eph up` or `eph dev` reuses the
  prewarmed dependency services instead of restarting them. `eph dev` on exit
  leaves the tier it adopted running, warm for the next command.
- `--role dep` brings up the dependency role and its dependency closure only,
  never the `app`. Add `--skip-hooks` to prewarm without running `post-start`
  seeding; the plain form above runs it.
- There is no `eph hooks install`: roles are user-defined names, so
  substitute your own. For a personal, cross-repo version put the same block
  in `~/.claude/settings.json` instead.
- Optional: a `SessionEnd` hook running `eph down --role dep` stops the tier
  when a session ends. The default is to leave it warm for reuse.

See [Recipes](recipes.md#prewarm-dependency-services-on-claude-code-session-start)
for the full write-up.

## Command cheat sheet

| Command | Effect |
|---------|--------|
| `eph up [svc...] [--role R]...` | Start all / selected services. In roles mode, a positional service also selects the dependency roles below its role, without selecting peer services in its own role. `--role R` (repeatable) selects the whole role plus its dependency closure. Runs each selected service's `pre-start` just before creation, pulls/builds, waits for health, then runs every selected `post-start`. Both hook kinds run on **every** `eph up`; a failing hook aborts the `up`. `--skip-hooks` skips both. Serializes with any other `eph up`/`down`/`clean` in the workspace. |
| `eph down [--rm \| -r] [svc...] [--role R]...` | Stop all / selected services. In roles mode, a positional service also selects the dependent roles above its role, without selecting peer services in its own role. `--role R` selects the whole role plus all dependent roles. `--rm` also removes containers. Compose is always fully torn down. Runs `pre-stop` before each stop and `post-stop` after; failures abort. A bare `eph down` also tears down services remembered in state but removed or renamed in `.eph`. |
| `eph clean` | Full reset: remove containers + named volumes + state. Deletes data. Also sweeps recorded-but-renamed/deleted services and any leftover `eph-<short_id>-*` container/volume Docker still has. Prints **measured** counts (what was actually stopped/removed, not what is declared); a never-started workspace reports zeros. Runs teardown hooks like `eph down`; `--skip-hooks` bypasses them. |
| `eph system prune [--dry-run] [--compatibility-v042] [--force-live] [-y\|--yes]` | Global prune of resources whose recorded workspace path is missing or empty. Verifies `run=` process identity before killing a PID; warns and skips when it cannot. Skips (does not force-kill) a stale-pathed workspace that still has a running container or live process unless `--force-live`; confirms before deleting unless `--dry-run`, `--yes`, or nothing would be removed (required when stdin is not a terminal). |
| `eph dev [svc] [--clean] [--watch GLOB]... [--skip-hooks]` | Foreground the stack for a preview server: up + seed + foreground the `run=` app; teardown of what it started on stop. Hooks are interleaved exactly like `eph up` (each backing service's `pre-start` runs right before it starts, the foreground app's `pre-start` runs right before it starts, then every service's `post-start` runs once everything is up). `--skip-hooks` skips all four hook phases, matching `eph up --skip-hooks` / `eph down --skip-hooks`. |
| `eph run <cmd>...` | Run a command in the workspace root with resolved env + `EPH_*` metadata. Refuses to launch if any top-level reference is unresolved. Every token after `run` belongs to the command, including flag-shaped tokens; no `--` is needed. Exits with the child's native status. |
| `eph logs [svc] [-f] [-n N]` | Show logs. No svc: all services interleaved, each line tagged `[name]`. One svc: raw. Works even for stopped services. `-f` follows. |
| `eph status` | Running services and ports. |
| `eph env [-f export\|fish\|powershell\|json]` | Print resolved top-level env vars. Unresolved shell variables are explicitly unset before the emitted script fails; JSON omits them. Both forms warn on stderr and exit non-zero. `--format json` keys follow declaration order. |
| `eph check` | Validate `.eph` (no Docker). |
| `eph info` | Workspace id / prefix / paths (no Docker). |
| `eph skills install` | Install this guidance as a discoverable agent skill (`.claude/skills`, `.agents/skills`). No Docker. Warns on stderr and installs into the current directory if run outside a git repo. |
| `eph skills check` | Verify the installed skill is current (non-zero exit on drift). No Docker. |
| `eph update [--check]` | Self-update to the latest release (checksum-verified). |
| `-v` / `--verbose` | Debug logging to stderr. |

To persist this guidance as a skill your agent discovers automatically on
every checkout, run `eph skills install` and commit the written files. It
bundles a `using-eph` skill (the same material as this page) into
`.claude/skills/` and `.agents/skills/`. Re-run it after upgrading `eph` to
refresh the text.

Output is on stdout; logs on stderr. Unknown service or format names are
errors.

## `.eph` file cheat sheet

INI-with-`.env`. Top-level `KEY=VALUE` are shell env vars; `[name]` sections
are services. Comments are own-line only (`#`); a `#` after a value is part of
the value, so **the example below contains no inline comments**. Do not add
them.

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

Line by line: `image=` is the source (one of image/dockerfile/compose/run);
`port=` is a container port published on a random host port; `env.X` is set
**inside the container**; `volume=name:/path` is a per-workspace named
volume; `healthcheck` for an image service runs without a shell
(whitespace-split, `docker exec`); `post-start` runs on the host via the
platform shell (`sh -c` on Unix, `cmd /C` on Windows) after every service is
healthy; `[env]` switches back to top-level variables (see below); the
trailing `DATABASE_URL=` is a **shell** env var emitted by `eph env`, with
`${postgres.port}` filled in at runtime.

**Sections do not end at blank lines.** A bare `KEY=VALUE` line directly after
a service section is a parse error, not a silent trailing variable. Top-level
variables (what `eph env` prints) only parse in two places: above the first
section, or inside a reserved `[env]` section (which may repeat). `env.KEY=`
inside a service section is a different thing entirely: it sets a variable
**in the container**, not your shell. Writing an UPPERCASE key straight into a
service section, meaning it for the shell, is the single most common mistake
when generating a `.eph` file: `eph check` rejects it and names both fixes
(`env.KEY=` for the container, `[env]` for the shell).

### Service sources (exactly one per section; a second one is a parse error)

| Key | Meaning |
|-----|---------|
| `image=` | Run a Docker image. |
| `dockerfile=` (+ `context=`) | Build a local image (paths from workspace root). |
| `compose=` (+ `expose.<alias>=`) | Delegate to a Compose file. Use `<compose-service>:<port>` when the target differs from the alias. `port=` and `port.<name>=` are illegal here. |
| `run=` | Host process via the platform shell. Numeric ports are NOT remapped (declared value reported as-is); `port=auto` makes eph allocate and inject the port. |

### Properties

| Key | Notes |
|-----|-------|
| `port=` / `port.<name>=` | Single / named ports. `auto` is valid for `run=` services only. Illegal on `compose=` (use `expose.<name>=`). |
| `env.<KEY>=` | Container env (not shell env). One value per distinct `KEY`. May contain `${service.property}`, resolved against running services for every source (`image`, `dockerfile`, `compose`, `run`) at the moment that service starts. |
| `volume=` | `name:/path` = named volume; `./host:/path` or `/abs:/path` = bind mount. Repeatable; only legal for `image=` and `dockerfile=`. |
| `role=` | Tier name for roles mode; requires a `roles_order` listing every role. |
| `command=` | Override container CMD (shell-word split, no shell). Only legal for `image=`/`dockerfile=`; a parse error on `run=`/`compose=`. |
| `healthcheck=` | image/dockerfile: no shell. run/compose: platform shell (`sh -c` / `cmd /C`). |
| `ready-timeout=` | Non-zero seconds (default 30; compose 60); requires `healthcheck=`. |
| `pre-start=` / `post-start=` / `pre-stop=` / `post-stop=` | Lifecycle hooks. Host platform shell in workspace root; repeatable; run with the resolved env + `EPH_*` metadata + the service's `env.X` injected. `pre-start` runs just before its service is created (no own port yet); `post-start` after all services are healthy. Both run on every `up`; failures abort. `pre-stop` runs before a stop (failure aborts, service left running); `post-stop` after (failure aborts the rest of teardown). |

Every property above except the four marked repeatable is single-valued: a
second occurrence is a parse error, as is an unknown property (the error
lists every known one), an invalid or duplicate service/port name, or an
empty value (everything except `env.<KEY>=`, where empty is legal).

### Interpolation (resolved against running services)

| Ref | Value |
|-----|-------|
| `${svc.port}` | Assigned host port (single-port). |
| `${svc.port.name}` | Named port. |
| `${svc.host}` | `localhost`. |

Unknown services, unknown properties, missing named ports, and ambiguous bare
ports are parse errors. A valid reference to a stopped service fails closed at
runtime: hooks, `eph run`, and service startup do not receive a raw placeholder.
`eph env` unsets affected shell variables and fails; JSON omits them and fails.
Compose aliases use `expose.<alias>=<compose-service>:<port>` and resolve as
`${svc.port.<alias>}`.

Resolved values are **host-facing** (`${svc.port}` is the host's loopback
port). That is correct for a hook, `eph run`, or a `run=` process, all of
which execute on the host, but usually wrong for one container reaching
another: a container's own `env.X=...${sibling.port}` resolves to a
`localhost:PORT` string that, from inside that container, points back at
itself. Reach a sibling container from inside another container via
`host.docker.internal` or a shared Docker network, not through this
interpolation.

## Behaviors that matter

- **Idempotent up reconciles configuration.** A resource is reused or restarted
  only when its canonical runtime fingerprint still matches. Source, immutable
  image, port, resolved environment, volume, health, build, and command drift
  removes the old backend and recreates it. Dockerfile sources build through
  Docker's cache on every `up`. Reused services rerun declared health checks;
  failed starts are removed. Hooks still run on every `up`.
- **Ports are random and change per create.** Never hardcode; always go
  through `eph env`.
- **Image health checks have no shell**: one whitespace-split command, no
  pipes / `&&` / `$VAR` / quoted spaces. The binary must exist in the image.
- **Unresolved runtime references fail closed.** Hooks, services, health
  checks, and `eph run` never receive a raw eph placeholder.
- **Isolation by path**: two checkouts = different containers, volumes,
  ports.
- **`compose` is thin** but tracked (by project label): `down` and
  `down --rm` both fully tear it down. Compose services cannot declare `.eph`
  volumes, and `clean` does not remove volumes defined inside the Compose file.
- **No `eph init`**: author `.eph` by hand.
- **Windows runs natively**: `run=`, hooks, and shell health checks go
  through `cmd /C` (vs `sh -c` on Unix), so command strings may need a
  `cmd`-compatible form. WSL keeps POSIX command strings working.
- **State survives crashes.** `state.json` is saved after each service
  starts (not once at the end), so a failed `eph up` still leaves `eph down`
  able to find and stop whatever did start. A corrupt `state.json` is
  quarantined to `state.json.corrupt` with a warning rather than blocking the
  command. `eph up`/`down`/`clean` on one workspace serialize against each
  other via an OS lock that a crashed process can never leave stuck.

## Safe defaults for automation

- Validate before acting: `eph check`.
- Start and load in CI in the **same** step:
  `eph up && eval "$(eph env)" && <cmd>`.
- Tear down in an always-run step: `eph clean`.
- Treat `.eph` as possibly containing dev credentials; do not print it to
  logs or commit one with real secrets.
