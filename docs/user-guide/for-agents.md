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
DATABASE_URL=$(eph env -f json | jq -r .DATABASE_URL)
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
| `eph up [svc...] [--role R]...` | Start all / named services. `--role R` (repeatable) adds a role plus its dependency closure (needs a `roles_order`); combines with names. Runs each service's `pre-start` just before it is created, pulls/builds, waits for health, then runs every `post-start`. Both hook kinds run on **every** `eph up`; a failing hook aborts the `up`. `--skip-hooks` skips both. |
| `eph down [--rm \| -r] [svc...] [--role R]...` | Stop all / named. `--role R` (repeatable) adds a role plus everything that depends on it. `--rm` also removes containers. Compose is always fully torn down. Runs `pre-stop` before each stop and `post-stop` after; a failing `pre-stop` aborts with the service left running, a failing `post-stop` aborts the rest. `--skip-hooks` bypasses both. |
| `eph clean` | Full reset: remove containers + named volumes + state. Deletes data. Runs teardown hooks like `eph down`; `--skip-hooks` bypasses them. |
| `eph system prune [--dry-run] [--compatibility-v042]` | Global prune of resources whose recorded workspace path is missing or empty. Verifies `run=` process identity before killing a PID; warns and skips when it cannot. |
| `eph dev [svc] [--clean] [--watch GLOB]...` | Foreground the stack for a preview server: up + seed + foreground the `run=` app; teardown of what it started on stop. |
| `eph run <cmd>...` | Run a command in the workspace root with the resolved env + `EPH_*` metadata. Exits with the command's code. |
| `eph logs [svc] [-f] [-n N]` | Show logs. No svc: all services interleaved, each line tagged `[name]`. One svc: raw. Works even for stopped services. `-f` follows. |
| `eph status` | Running services and ports. |
| `eph env [-f export\|fish\|json]` | Print resolved top-level env vars (stdout). |
| `eph check` | Validate `.eph` (no Docker). |
| `eph info` | Workspace id / prefix / paths (no Docker). |
| `eph skills install` | Install this guidance as a discoverable agent skill (`.claude/skills`, `.agents/skills`). No Docker. |
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

DATABASE_URL=postgres://dev@localhost:${postgres.port}/app
```

Line by line: `image=` is the source (one of image/dockerfile/compose/run);
`port=` is a container port published on a random host port; `env.X` is set
**inside the container**; `volume=name:/path` is a per-workspace named
volume; `healthcheck` for an image service runs without a shell
(whitespace-split, `docker exec`); `post-start` runs on the host via the
platform shell (`sh -c` on Unix, `cmd /C` on Windows) after every service is
healthy; the trailing `DATABASE_URL=` is a **shell** env var emitted by
`eph env`, with `${postgres.port}` filled in at runtime.

### Service sources (one per section; if several are given, the last wins)

| Key | Meaning |
|-----|---------|
| `image=` | Run a Docker image. |
| `dockerfile=` (+ `context=`) | Build a local image (paths from workspace root). |
| `compose=` (+ `expose.<name>=`) | Delegate to a Compose file. |
| `run=` | Host process via the platform shell. Numeric ports are NOT remapped (declared value reported as-is); `port=auto` makes eph allocate and inject the port. |

### Properties

| Key | Notes |
|-----|-------|
| `port=` / `port.<name>=` | Single / named ports. `auto` is valid for `run=` services only. |
| `env.<KEY>=` | Container env (not shell env). |
| `volume=` | `name:/path` = named volume; `./host:/path` or `/abs:/path` = bind mount. |
| `role=` | Tier name for roles mode; requires a `roles_order` listing every role. |
| `command=` | Override container CMD (shell-word split, no shell). |
| `healthcheck=` | image/dockerfile: no shell. run/compose: platform shell (`sh -c` / `cmd /C`). |
| `ready-timeout=` | Seconds (default 30; compose 60). |
| `pre-start=` / `post-start=` / `pre-stop=` / `post-stop=` | Lifecycle hooks. Host platform shell in workspace root; repeatable; run with the resolved env + `EPH_*` metadata + the service's `env.X` injected. `pre-start` runs just before its service is created (no own port yet); `post-start` after all services are healthy. Both run on every `up`; failures abort. `pre-stop` runs before a stop (failure aborts, service left running); `post-stop` after (failure aborts the rest of teardown). |

### Interpolation (resolved against running services)

| Ref | Value |
|-----|-------|
| `${svc.port}` | Assigned host port (single-port). |
| `${svc.port.name}` | Named port. |
| `${svc.host}` | `localhost`. |

Unresolved refs (stopped service, typo) are left verbatim. All running
services resolve, including `compose` (tracked by the
`com.docker.compose.project` label); reference a compose service's
`expose.<name>=` port as `${svc.port.<name>}`.

## Behaviors that matter

- **Idempotent up** (image/dockerfile): running is reused,
  stopped-but-present is restarted, absent is created fresh. `run` reuses a
  live PID or respawns; `compose` delegates to `docker compose up -d`.
  Regardless of path, hooks run for every service on **every** `eph up`; keep
  hooks idempotent, or use `eph run` for one-off work.
- **Ports are random and change per create.** Never hardcode; always go
  through `eph env`.
- **Image health checks have no shell**: one whitespace-split command, no
  pipes / `&&` / `$VAR` / quoted spaces. The binary must exist in the image.
- **Isolation by path**: two checkouts = different containers, volumes,
  ports.
- **`compose` is thin** but tracked (by project label): `down` and
  `down --rm` both fully tear it down; `clean` removes only `.eph` `volume=`
  named volumes, not Compose-internal ones.
- **No `eph init`**: author `.eph` by hand.
- **Windows runs natively**: `run=`, hooks, and shell health checks go
  through `cmd /C` (vs `sh -c` on Unix), so command strings may need a
  `cmd`-compatible form. WSL keeps POSIX command strings working.

## Safe defaults for automation

- Validate before acting: `eph check`.
- Start and load in CI in the **same** step:
  `eph up && eval "$(eph env)" && <cmd>`.
- Tear down in an always-run step: `eph clean`.
- Treat `.eph` as possibly containing dev credentials; do not print it to
  logs or commit one with real secrets.
