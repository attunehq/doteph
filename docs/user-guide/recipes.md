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
  passes it as `$PORT`. `eph dev` binds the foreground app's `port=auto` to that
  exact port, so the app's `${web.port}` (and the `healthcheck` and any
  `${web.port}` env) all resolve to the port the preview is watching. Do not give
  the app a fixed port as well.
- **Setup runs once per launch.** `eph dev` does `eph up` first, so postgres is
  healthy and `post-start` (migrate/seed) has run before the app starts.
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
