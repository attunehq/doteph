---
title: "Recipes"
summary: "End-to-end setups: Compose migration, seeding, CI, prewarming, and secrets."
order: 7
---

# Recipes

Practical, end-to-end setups. Each assumes you have read
[Core Concepts](concepts.md). The preview-server and watch-mode workflows
live in [Running Your App](run-your-app.md).

## Migrating from Docker Compose

`eph` and Compose describe the same services; the mapping is mechanical.

| docker-compose.yml | `.eph` |
|--------------------|--------|
| `services: { postgres: ... }` | `[postgres]` |
| `image: postgres:16` | `image=postgres:16` |
| `build: { context: ., dockerfile: X }` | `dockerfile=X` + `context=.` |
| `ports: ["5432:5432"]` | `port=5432` (the host port becomes automatic) |
| `environment: { POSTGRES_USER: dev }` | `env.POSTGRES_USER=dev` |
| `volumes: ["pgdata:/var/lib/..."]` | `volume=pgdata:/var/lib/...` |
| `command: server /data` | `command=server /data` |
| `healthcheck: { test: [...] }` | `healthcheck=...` (one command, no shell for image services) |

Worked example. This Compose file:

```yaml
services:
  postgres:
    image: postgres:16-alpine
    ports: ["5432:5432"]
    environment:
      POSTGRES_USER: dev
      POSTGRES_PASSWORD: dev
      POSTGRES_DB: myapp
    volumes:
      - pgdata:/var/lib/postgresql/data
  redis:
    image: redis:7-alpine
    ports: ["6379:6379"]
volumes:
  pgdata:
```

becomes:

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
volume=pgdata:/var/lib/postgresql/data
healthcheck=pg_isready -U dev

[redis]
image=redis:7-alpine
port=6379
healthcheck=redis-cli ping

[env]
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
REDIS_URL=redis://localhost:${redis.port}
```

What you gain: the fixed host ports (`5432:5432`) become automatic, so
multiple checkouts stop fighting over them, and services only run when you
`eph up`.

If a subsystem is genuinely complex (many interdependent containers), do not
translate it. Keep the Compose file and reference it with `compose=`; see
[Defining Services](services.md#compose-delegate-to-docker-compose).

## Seeding a database

Three approaches, by when you want the seed to run.

**A `post-start` hook** runs your project's own migrate and seed commands on
the host after the container is healthy, on every `eph up`. The hook already
has eph's resolved environment, so `$DATABASE_URL` is set without any `eval`:

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
healthcheck=pg_isready -U dev
post-start=npm run db:migrate
post-start=npm run db:seed

[env]
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
```

Because it runs on **every** `eph up`, keep these commands idempotent: a
migration that no-ops when applied, an `INSERT ... ON CONFLICT` seed. A
failing `post-start` aborts the `up`. For a destructive re-seed from scratch,
recreate the data volume with `eph clean && eph up`, or run the seed on demand
with `eph run` (next).

**On demand with `eph run`**, for a re-seed you repeat whenever you want. It
gets the same environment (`$DATABASE_URL`, `EPH_*`) a `post-start` hook
would:

```sh
eph run npm run db:migrate
eph run npm run db:seed
eph run psql "$DATABASE_URL" -f fixtures.sql   # $DATABASE_URL from your shell
```

Unlike a hook, this runs only when you invoke it, so it is the simplest way to
reset data without recreating the container.

**Init scripts via bind mount**: the official Postgres and MySQL images run
any `*.sql` or `*.sh` in `/docker-entrypoint-initdb.d` on first initialization
of the data volume:

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
volume=pgdata:/var/lib/postgresql/data
volume=./db/init:/docker-entrypoint-initdb.d
healthcheck=pg_isready -U dev
```

These run only when the data volume is empty, so `eph clean` (which deletes
the named volume) is how you trigger a re-seed.

## Prewarm dependency services on Claude Code session start

When a coding agent opens a worktree, its dev services are not running, and
the dependency tier (databases, caches) is the slow, known-good part: image
pulls, migrations, seeds. Warm it once on session start and reuse it. A Claude
Code **SessionStart hook** can bring up just the dependency tier and inject
its connection env before the agent's first command, without starting the
first-party app (which could bind preview ports or cause side effects the
agent did not ask for).

This needs a `.eph` file that uses
[roles](eph-file.md#roles-and-ordering): tag the backing services `role=dep`,
the app `role=app`, and declare `roles_order=dep,app`.

```ini
roles_order=dep,app

[postgres]
image=postgres:16-alpine
role=dep
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
healthcheck=pg_isready -U dev
post-start=npm run db:migrate

[web]
run=npm run dev
role=app
port=auto
env.PORT=${web.port}

[env]
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
```

The hook script runs `eph up --role dep` (which starts the `dep` tier and its
dependency closure, never the app), then appends `eph env` to the file named
by `$CLAUDE_ENV_FILE`. Claude Code sources that file, so subsequent Bash tool
calls inherit `DATABASE_URL` and the rest:

```sh
#!/usr/bin/env bash
# .claude/hooks/eph-prewarm.sh
# SessionStart hook: prewarm dependency services and inject their env.
eph up --role dep >/dev/null 2>&1 || exit 0
[ -n "$CLAUDE_ENV_FILE" ] && eph env >> "$CLAUDE_ENV_FILE"
```

Make it executable (`chmod +x .claude/hooks/eph-prewarm.sh`) and wire it in
`.claude/settings.json`, so everyone who opens the repo or a worktree of it
gets the hook:

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

How it behaves:

- **The app is left alone.** `--role dep` resolves to the dependency role and
  its dependency closure only. The `run=` app never starts, so no preview port
  is bound and no app-side side effects fire on session start.
- **The tier is reused, not restarted.** `eph up` is idempotent, so a later
  `eph up` or `eph dev` adopts the already-running dependency services. On
  exit, `eph dev` tears down only the app it started and leaves the prewarmed
  tier hot for the next session.
- **Seeding is included.** The plain `eph up --role dep` runs each
  dependency's `post-start` (migrations, seeds). Add `--skip-hooks` to prewarm
  without seeding.
- **No install command.** Roles are names you choose, so there is deliberately
  no `eph hooks install`. Copy the recipe and substitute your dependency role
  name. For a personal version that follows you across repos, put the same
  `SessionStart` block in `~/.claude/settings.json` instead of the project
  file.
- **Optional cleanup on exit.** To stop the tier when a session ends rather
  than leaving it warm, add a `SessionEnd` hook running `eph down --role dep`.
  Leaving it running (the default here) is usually what you want, so the next
  session reuses it.

## Bring up only one tier

Once a `.eph` file defines [roles](eph-file.md#roles-and-ordering), `--role`
starts or stops a tier and its closure without naming individual services:

```sh
eph up --role dep      # dependency services (plus anything they depend on), not the app
eph up --role app      # the app plus every role it depends on (here: dep, then app)
eph down --role dep    # stop the dep tier AND everything that depends on it
```

`eph up --role` resolves the **dependency** closure (the role plus what it
needs below it); `eph down --role` resolves the **dependent** closure (the
role plus everything above it that would break without it), torn down in
reverse start order. Both combine with positional service names. This is what
the prewarm hook above uses to start the backing tier alone.

## Multiple checkouts side by side

This is what `eph` is built for. Clone the same repo twice:

```sh
git clone git@github.com:you/app.git app
git clone git@github.com:you/app.git app-experiment
```

Run `eph up` in each. Because the workspace ID is derived from the path, each
checkout gets its own containers, its own volumes, and its own ports: no
conflicts, no shared data. `eph status` in either directory shows only that
checkout's services, and `eph info` shows the differing short IDs.

When you later delete a checkout (a finished worktree, an abandoned
experiment), its containers and volumes do not die with the directory. Run
[`eph system prune`](command-reference.md#eph-system-prune---dry-run---compatibility-v042---force-live--y---yes)
from anywhere to sweep up resources belonging to workspaces whose directory no
longer exists.

## Using `eph` in CI

`eph` works in any CI that provides Docker. The pattern:

```yaml
# GitHub Actions (sketch)
steps:
  - uses: actions/checkout@v4
  - run: cargo install --path . # or download a release binary
  - run: eph up
  - run: |
      eval "$(eph env)"
      <your test command>
  - run: eph clean
    if: always()
```

Notes:

- Each `run:` step is a fresh shell, so `eval "$(eph env)"` must be in the
  same step as the command that needs the variables (or export them to the CI
  environment file).
- `eph up` blocks until health checks pass, so tests never start before the
  services are ready. No manual `sleep`.
- Use `eph clean` in an always-run step to release resources.

## Handling secrets

A `.eph` file can contain credentials (for example `env.POSTGRES_PASSWORD`),
and `eph env` prints top-level variables to stdout. Treat the file
accordingly:

- Prefer **throwaway, dev-only** credentials. The whole point is ephemeral
  local services; they do not need real secrets.
- If a `.eph` file must hold something sensitive, add it to `.gitignore` and
  commit a `.eph.example` template instead.
- Never commit a `.eph` file containing real secrets.

## Keeping data vs. starting fresh

| You want to... | Run |
|----------------|-----|
| Pause for the day, keep everything | `eph down` |
| Free the containers, keep the data | `eph down --rm` |
| Re-run migrations or seeds against the running data | `eph run <your migrate/seed cmd>` |
| Wipe data and start completely clean | `eph clean` then `eph up` |
| Sweep up after deleted checkouts | `eph system prune` |

## Next

[Troubleshooting](troubleshooting.md) covers the failure modes you will
eventually meet, and how to read what eph tells you.
