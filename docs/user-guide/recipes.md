# Recipes

Practical, end-to-end setups. Each assumes you have read
[Core Concepts](concepts.md).

## Migrating from Docker Compose

`eph` and Compose describe the same services; the mapping is mechanical.

| docker-compose.yml | `.eph` |
|--------------------|--------|
| `services: { postgres: ... }` | `[postgres]` |
| `image: postgres:16` | `image=postgres:16` |
| `build: { context: ., dockerfile: X }` | `dockerfile=X` + `context=.` |
| `ports: ["5432:5432"]` | `port=5432` (host port becomes automatic) |
| `environment: { POSTGRES_USER: dev }` | `env.POSTGRES_USER=dev` |
| `volumes: ["pgdata:/var/lib/..."]` | `volume=pgdata:/var/lib/...` |
| `command: server /data` | `command=server /data` |
| `healthcheck: { test: [...] }` | `healthcheck=...` (single command, no shell for image services) |

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

DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
REDIS_URL=redis://localhost:${redis.port}
```

What you gain: fixed host ports (`5432:5432`) become automatic, so multiple
checkouts stop fighting over ports, and services only run when you `eph up`.

If a subsystem is genuinely complex (many interdependent containers), you do not
have to translate it - keep the Compose file and reference it with `compose=`.
See [Defining Services](services.md#compose---delegate-to-docker-compose).

## Seeding a database

Three approaches:

**Post-start hook** - runs your project's own migrate/seed commands on the host
after the container is healthy. The hook already has eph's resolved environment,
so `$DATABASE_URL` is set without any `eval`:

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

DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
```

`post-start` runs on **every** `eph up`, so keep these commands idempotent (a
migration that no-ops when already applied, an `INSERT ... ON CONFLICT` seed). A
failing `post-start` aborts the `up`. For a destructive re-seed from scratch,
recreate the data volume with `eph clean && eph up`, or run the seed on demand
with `eph run` (below).

**On demand with `eph run`** - for a re-seed you can repeat any time, skip the
hook and run the command directly. It gets the same environment (`$DATABASE_URL`,
`EPH_*`) as a `post-start` hook would:

```sh
eph run npm run db:migrate
eph run npm run db:seed
eph run psql "$DATABASE_URL" -f fixtures.sql   # $DATABASE_URL from your shell
```

Unlike `post-start`, this runs every time you invoke it, so it is the simplest
way to reset data without recreating the container.

**Init scripts via bind mount** - the official Postgres/MySQL images run any
`*.sql`/`*.sh` in `/docker-entrypoint-initdb.d` on first initialization of the
data volume:

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

These run only when the data volume is empty, so `eph clean` (which deletes the
named volume) is how you trigger a re-seed.

## Claude Desktop preview servers

[Claude Desktop](https://code.claude.com/docs/en/desktop#configure-preview-servers)
launches your dev server from `.claude/launch.json` and watches its port for the
in-app preview. Each configuration runs a single foreground command and offers no
separate setup or teardown hook, so backing services and seeding have nowhere to
hang off. `eph dev` is one foreground process that brings the stack up, runs
`post-start` (your seeding), foregrounds the app, and tears everything down when
the preview server stops it.

Model the app as a `run=` service with `port=auto`, alongside its backing
services and seeding:

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
healthcheck=pg_isready -U dev

[web]
run=npm run dev
port=auto
env.PORT=${web.port}
healthcheck=curl -sf http://localhost:${web.port}/healthz
post-start=npm run db:migrate

DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
```

Then point the preview server at `eph dev`:

```jsonc
// .claude/launch.json
{
  "version": "0.0.1",
  "configurations": [
    {
      "name": "web",
      "runtimeExecutable": "eph",
      "runtimeArgs": ["dev"],
      "port": 3000,
      "autoPort": true
    }
  ]
}
```

How the pieces line up:

- **`autoPort` and the port.** The preview server picks a free host port and
  passes it as `$PORT`, then polls it and reveals the app the instant it accepts a
  connection. `eph dev` runs the app on its own internal `port=auto` and opens
  `$PORT` as a forwarding gate to it, so `${web.port}` (and the `healthcheck` and
  any `${web.port}` env) resolve to the app's real port. Do not give the app a
  fixed port as well.
- **The preview waits for seeding, not just for the port.** The gate is the point:
  `eph dev` opens `$PORT` only *after* `post-start` hooks run, so the preview
  cannot go live while a slow seed is still filling the database. Without it the
  app would bind `$PORT` itself and the preview would show an empty app the moment
  the server could answer its health check, often tens of seconds before the seed
  finished.
- **Setup runs once per launch.** `eph dev` brings the backing services up, starts
  the app, then runs every service's `post-start` (migrate/seed) before opening the
  gate, so the first thing the preview sees is a seeded app.
- **The app is interactive.** `eph dev` wires its own stdin, stdout, and stderr
  straight through to the app, so the dev server's output reaches the preview
  console live and anything the preview server writes to stdin reaches the app.
  eph's own startup lines go to stderr to stay out of the app's stdout.
- **Teardown on stop.** When Claude stops the preview server, `eph dev` runs
  `eph down` (keeps the database for a fast relaunch). Claude Desktop restarts the
  preview server during a session, so `down` is the default: a `clean` per restart
  would re-create and re-seed every time. Use `eph dev --clean` (`runtimeArgs:
  ["dev", "--clean"]`) only when you want a pristine database on every launch.
- **`eph` must be on the app's PATH.** The desktop app does not always inherit
  your shell `PATH` (notably a macOS Dock/Finder launch), so put `eph` somewhere
  the app sees or use an absolute path in `runtimeExecutable`.

If you would rather keep the app *out* of eph and let `launch.json` run it
directly, model only the backing services in `.eph`, run `eph up` yourself, and
launch the app through `eph run` so it still gets the resolved environment:
`"runtimeExecutable": "eph", "runtimeArgs": ["run", "npm", "run", "dev"]`. Here
`launch.json` owns the app's port and `eph run` passes `$PORT` through, but setup
(`eph up`) and teardown (`eph down`) are no longer automatic, which is the manual
work `eph dev` does for you.

## Prewarm dependency services on Claude Code session start

When an agent opens a worktree, its dev services are usually not running, and the
dependency tier (databases, caches) is the slow, known-good part to start: image
pulls, migrations, seeds. Warm it once on session start and reuse it. A Claude Code
**SessionStart hook** can bring up just the dependency tier and inject its
connection env before the agent's first command, without starting the first-party
app (which could bind preview ports or cause surprising side effects).

This needs a `.eph` file that uses [roles](eph-file.md#roles-and-ordering): tag the
backing services `role=dep`, the app `role=app`, and declare `roles_order=dep,app`.

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

DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
```

The hook script runs `eph up --role dep` (which starts the `dep` tier and its
dependency closure, never the app), then appends `eph env` to the file named by
`$CLAUDE_ENV_FILE`. Claude Code sources that file, so subsequent Bash tool calls
inherit `DATABASE_URL` and the rest:

```sh
#!/usr/bin/env bash
# .claude/hooks/eph-prewarm.sh
# SessionStart hook: prewarm dependency services and inject their env.
eph up --role dep >/dev/null 2>&1 || exit 0
[ -n "$CLAUDE_ENV_FILE" ] && eph env >> "$CLAUDE_ENV_FILE"
```

Make it executable (`chmod +x .claude/hooks/eph-prewarm.sh`) and wire it in
`.claude/settings.json` so it applies to everyone who opens the repo or a worktree
of it:

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

- **The app is left alone.** `--role dep` resolves to the dependency role and its
  dependency closure only. The `run=` app never starts, so no preview port is bound
  and no app-side side effects fire on session start.
- **The tier is reused, not restarted.** `eph up` is idempotent (see
  [Core Concepts](concepts.md#the-service-lifecycle)), so when you later run
  `eph up` or `eph dev`, the already-running dependency services are reused. `eph
  dev` on exit tears down only the app it foregrounded and leaves the prewarmed tier
  running, so it stays warm across sessions and dev runs.
- **Seeding is included.** The plain `eph up --role dep` runs each dependency's
  `post-start` (migrations, seeds). Add `--skip-hooks` if you want the services up
  without seeding.
- **No install command.** Roles are names you choose, so there is deliberately no
  `eph hooks install`: copy the recipe and substitute your dependency role name. For
  a personal version that follows you across repos, put the same `SessionStart`
  block in `~/.claude/settings.json` instead of the project file.
- **Optional cleanup on exit.** To stop the tier when a session ends rather than
  leaving it warm, add a `SessionEnd` hook running `eph down --role dep`. Leaving it
  running (the default here) is usually what you want, so the next session reuses it.

## Bring up only one tier

Once a `.eph` file defines [roles](eph-file.md#roles-and-ordering), `--role` starts
or stops a tier and its closure without naming individual services:

```sh
eph up --role dep      # dependency services (+ anything they depend on), not the app
eph up --role app      # the app plus every role it depends on (here: dep, then app)
eph down --role dep    # stop the dep tier AND everything that depends on it
```

`eph up --role` resolves the **dependency** closure (the role plus what it needs
below it); `eph down --role` resolves the **dependent** closure (the role plus
everything above it that would break without it), torn down in reverse start order.
Both combine with positional service names. This is what the prewarm hook above uses
to start the backing tier alone.

## Multiple checkouts side by side

This is what `eph` is built for. Clone the same repo twice:

```sh
git clone git@github.com:you/app.git app
git clone git@github.com:you/app.git app-experiment
```

Run `eph up` in each. Because the workspace ID is derived from the path, each
gets its own containers, its own volumes, and its own ports - no conflicts, no
shared data. `eph status` in either directory shows only that checkout's
services. Verify with `eph info` (the short IDs differ).

## Using `eph` in CI

`eph` works in CI that provides Docker. The pattern:

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

- Each `run:` step is a fresh shell, so `eval "$(eph env)"` must be in the same
  step as the command that needs the variables (or export them to the CI
  environment file).
- `eph up` blocks until health checks pass, so your tests do not start before
  the services are ready - no manual `sleep`.
- Use `eph clean` in an always-run step to release resources.

## Handling secrets

A `.eph` file can contain credentials (for example `env.POSTGRES_PASSWORD`), and
`eph env` prints top-level variables to stdout. Treat the file accordingly:

- Prefer **throwaway, dev-only** credentials. The whole point is ephemeral local
  services; they do not need real secrets.
- If a `.eph` file must hold something sensitive, add it to `.gitignore` and
  commit a `.eph.example` template instead.
- Do not commit a `.eph` file containing real secrets.

## Keeping data vs. starting fresh

| You want to... | Run |
|----------------|-----|
| Pause for the day, keep everything | `eph down` |
| Free the containers, keep the data | `eph down --rm` |
| Re-run migrations/seeds against the running data | `eph run <your migrate/seed cmd>` |
| Wipe data and start completely clean | `eph clean` then `eph up` |
