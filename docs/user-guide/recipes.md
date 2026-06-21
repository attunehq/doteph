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

Two approaches:

**Post-start hook** - runs your project's own migrate/seed commands on the host
after the container is healthy:

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
```

Remember `post-start` runs when the container is **created**, not on every `up`.
To re-run from scratch: `eph down --rm && eph up`, or `eph clean && eph up`. See
[Troubleshooting](troubleshooting.md#post-start-hooks-did-not-run-again).

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
| Re-run `post-start` migrations on next `up` | `eph down --rm` then `eph up` |
| Wipe data and start completely clean | `eph clean` then `eph up` |
