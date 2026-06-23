# For Agents and Scripts

A terse, scannable reference for AI coding agents and automation working in a
repo that uses `eph`. Everything here is also explained, with rationale, in the
rest of the [user guide](README.md). If you are an agent, you can act from this
page alone.

## What eph is

A CLI that starts per-workspace dev services (Postgres, Redis, etc.) from a
`.eph` file in the project root. Containers are namespaced by a hash of the
workspace path; host ports are auto-assigned. It is `.env` for services.

## Detect and inspect

```sh
test -f .eph && echo "this project uses eph"   # a workspace has a .eph file
eph check        # validate the file, list services + env vars (no Docker)
eph info         # workspace id, container prefix, paths
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

## Command cheat sheet

| Command | Effect |
|---------|--------|
| `eph up [svc...]` | Start all / named services. Pulls/builds, waits for health, then runs `post-start` for every service on **every** `eph up`. A failing `post-start` aborts the `up`. `--skip-hooks` skips `post-start`. |
| `eph down [--rm \| -r] [svc...]` | Stop all / named. `--rm` (alias `-r`) also removes containers. Compose is always fully torn down. A failing `pre-stop` aborts the `down`; `--skip-hooks` bypasses `pre-stop`. |
| `eph clean` | Full reset: remove containers + named volumes + state. Deletes data. A failing `pre-stop` aborts it; `--skip-hooks` bypasses `pre-stop`. |
| `eph run <cmd>...` | Run a command in the workspace root with the resolved env + `EPH_*` metadata. Exits with the command's code. |
| `eph logs [svc] [-f] [-n N]` | Show logs (all services, or one). `run=` reads a captured log file; Docker/compose proxy `docker logs`. Shows even for stopped services. `-f` follows (one service). |
| `eph status` | Running services and ports. |
| `eph env [-f export\|fish\|json]` | Print resolved top-level env vars (stdout). |
| `eph check` | Validate `.eph` (no Docker). |
| `eph info` | Workspace id / prefix / paths. |
| `eph skills install` | Install this page as a discoverable agent skill into the repo (`.claude/skills`, `.agents/skills`). No Docker. |
| `eph skills check` | Verify the installed skill is current (non-zero exit on drift). No Docker. |
| `-v` / `--verbose` | Debug logging to stderr. |

To persist this guidance as a skill your agent discovers automatically on every
checkout, run `eph skills install` and commit the written files. It bundles a
`using-eph` skill (the same material as this page) into `.claude/skills/` and
`.agents/skills/`. Re-run it after upgrading `eph` to refresh the text.

Output is on stdout; logs on stderr. Unknown service/format names are errors.

## `.eph` file cheat sheet

INI-with-`.env`. Top-level `KEY=VALUE` are shell env vars; `[name]` sections are
services. Comments are own-line only (`#`); a `#` after a value is part of the
value, so **the example below contains no inline comments** - do not add them.

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
**inside the container**; `volume=name:/path` is a per-workspace named volume;
`healthcheck` for an image service runs without a shell (whitespace-split,
`docker exec`); `post-start` runs on the host via `sh -c` after the service is
healthy; the trailing `DATABASE_URL=` is a **shell** env var emitted by `eph
env`, with `${postgres.port}` filled in at runtime.

### Service sources (one per section; if several are given, the last wins)

| Key | Meaning |
|-----|---------|
| `image=` | Run a Docker image. |
| `dockerfile=` (+ `context=`) | Build a local image (paths from workspace root). |
| `compose=` (+ `expose.<name>=`) | Delegate to a Compose file. |
| `run=` | Host process via `sh -c`. Ports are NOT remapped (declared port used as-is). |

### Properties

| Key | Notes |
|-----|-------|
| `port=` / `port.<name>=` | Single / named ports. |
| `env.<KEY>=` | Container env (not shell env). |
| `volume=` | `name:/path` = named volume; `./host:/path` or `/abs:/path` = bind mount. |
| `command=` | Override container CMD (shell-word split, no shell). |
| `healthcheck=` | image/dockerfile: no shell. run/compose: `sh -c`. |
| `ready-timeout=` | Seconds (default 30; compose 60). |
| `post-start=` / `pre-stop=` | Host `sh -c` in workspace root; repeatable. Run with the resolved env + `EPH_*` metadata + the service's `env.X` injected. `post-start` runs on every `up` (after all services healthy); failure aborts `up`. `pre-stop` failure aborts `down`/`clean`. |

### Interpolation (resolved by `eph env`, running services only)

| Ref | Value |
|-----|-------|
| `${svc.port}` | Assigned host port (single-port). |
| `${svc.port.name}` | Named port. |
| `${svc.host}` | `localhost`. |

Unresolved refs (stopped service / typo) are left verbatim. All running services
resolve, including `compose` (tracked by the `com.docker.compose.project` label);
reference a compose service's `expose.<name>=` port as `${svc.port.<name>}`.

## Behaviors that matter

- **Idempotent up** (image/dockerfile): running -> reused; stopped-but-present ->
  restarted; absent -> created fresh. `run` reuses a live PID or respawns;
  `compose` delegates to `docker compose up -d`. Regardless of path, `post-start`
  runs for every service on **every** `eph up` -- keep hooks idempotent, or use
  `eph run` for one-off work.
- **Ports are random and change per create.** Never hardcode; always go through
  `eph env`.
- **Image health checks have no shell** - one whitespace-split command, no
  pipes/`&&`/`$VAR`/quoted-spaces. The binary must exist in the image.
- **Isolation by path**: two checkouts = different containers/volumes/ports.
- **`compose` is thin** but tracked (by project label): `down`/`down --rm` both
  fully tear it down (`--rm` is a no-op for it); `clean` removes only `.eph`
  `volume=` named volumes, not Compose-internal ones.
- **No `eph init`**: author `.eph` by hand.
- **Windows needs WSL** for `run=`, hooks, and shell health checks.

## Safe defaults for automation

- Validate before acting: `eph check`.
- Start and load in CI in the **same** step: `eph up && eval "$(eph env)" && <cmd>`.
- Tear down in an always-run step: `eph clean`.
- Treat `.eph` as possibly containing dev credentials; do not print it to logs
  or commit one with real secrets.
