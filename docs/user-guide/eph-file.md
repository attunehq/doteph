# The `.eph` File

The `.eph` file is the entire configuration for a workspace. It extends `.env`
syntax with INI-style `[sections]` for services. A plain `.env` file is already
a valid `.eph` file - you add services on top.

## Anatomy

```ini
# A top-level environment variable (exported by `eph env`)
APP_ENV=development

# A service definition
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev

# Top-level variables can interpolate running services
DATABASE_URL=postgres://dev@localhost:${postgres.port}/app
```

There are exactly two kinds of content:

- **Top-level environment variables** - `KEY=VALUE` lines outside any section.
  These are what `eph env` prints for your shell.
- **Service sections** - `[name]` followed by `property=value` lines. These
  define what `eph up` starts.

## Syntax rules

- One directive per line.
- `key=value` splits on the **first** `=`. Both sides are trimmed of
  surrounding whitespace.
- A value may be wrapped in a single matching pair of `'single'` or `"double"`
  quotes, which are stripped. Quotes are only needed to preserve leading or
  trailing spaces; they are otherwise optional.
- Within a service section, the order of properties does not matter, and
  interpolation refers to services by name so a service can be referenced from
  anywhere in the file. **Placement relative to sections does matter**, though:
  a top-level environment variable written after a `[section]` ends that section
  (see the reclassification note below). Blank lines and comments do **not** end
  a section. The conventional layout is top-level variables first, then service
  sections - or sections first, then a block of trailing variables.

### Comments

A comment is a line whose first non-whitespace character is `#`:

```ini
# This is a comment.
image=postgres:16-alpine
```

**Comments must be on their own line.** There are no inline/trailing comments -
a `#` after a value becomes part of the value:

```ini
port=5432            # WRONG: the value becomes "5432            # ..." and
                     # fails to parse as a port number
```

Write it as:

```ini
# the database port
port=5432
```

## Environment variables

Outside a section, every `KEY=VALUE` is a top-level environment variable:

```ini
APP_ENV=development
DEBUG=true
LOG_LEVEL=info
```

These are the variables `eph env` emits for your shell to load. They may contain
`${service.property}` interpolation (see below).

> Do not confuse these with `env.KEY=` *inside* a service section. Top-level
> variables go to **your shell**; `env.KEY=` goes **into the container**. They
> are separate.

## Service sections

A service is a `[bracketed]` section followed by its properties:

```ini
[redis]
image=redis:7-alpine
port=6379
healthcheck=redis-cli ping
```

Every service **must declare a source**: `image`, `dockerfile`, `compose`, or
`run`. Declare exactly one - if a section lists more than one, the last one wins
(this is not validated, so treat it as a mistake to avoid). A section with no
source is rejected at parse time (by `eph check` and before any `eph up`):

```
service 'redis' has no source defined (set one of image/dockerfile/compose/run)
```

The four source types are covered in detail in
[Defining Services](services.md).

### Service properties

| Property | Repeatable | Description |
|----------|:----------:|-------------|
| `role=` | no | The role (tier) this service belongs to, a free-form name you choose (e.g. `dep`, `app`). Optional, but once any service sets it every service must, and a `roles_order` must list the roles (see [Roles and ordering](#roles-and-ordering)). |
| `image=` | no | Docker image to pull and run. |
| `dockerfile=` | no | Path to a Dockerfile to build (relative to workspace). |
| `context=` | no | Build context for `dockerfile=` (defaults to the Dockerfile's directory). |
| `compose=` | no | Path to a Docker Compose file to delegate to. |
| `run=` | no | Shell command for a non-Docker service. |
| `command=` | no | Override the container's default command (`image`/`dockerfile` only). |
| `port=` | yes | A container port to publish on a random host port. For `run=` services, `port=auto` lets eph allocate a free host port and inject it (see [`run=` first-party app ports](services.md#first-party-app-ports-portauto)). |
| `port.<name>=` | yes | A **named** port (for multi-port services). `port.<name>=auto` is allowed for `run=` services. |
| `env.<KEY>=` | yes | An environment variable passed into the container. |
| `volume=` | yes | A volume mount: `name:/path` (named) or `./host:/path` (bind). |
| `healthcheck=` | no | Command that must succeed before the service is "ready". |
| `ready-timeout=` | no | Seconds to wait for the `healthcheck` (default 30; 60 for compose). Ignored when no `healthcheck` is set. |
| `pre-start=` | yes | Command run before the service is created. |
| `post-start=` | yes | Command run after the service becomes healthy. |
| `pre-stop=` | yes | Command run before the service is stopped. |
| `post-stop=` | yes | Command run after the service has stopped. |
| `expose.<name>=` | yes | For `compose=`: expose a port for interpolation. |

"Repeatable" means you can list the property multiple times and all values are
kept (for example several `post-start=` lines, run in order).

> Unknown property names are caught: a lowercase typo like `prot=5432` is a hard
> parse error. But an unknown key in `SCREAMING_SNAKE_CASE` is treated as a
> top-level environment variable (it ends the section), with a warning. So a
> miscased `HEALTHCHECK=...` silently becomes a global variable instead of a
> health check. See [Troubleshooting](troubleshooting.md#a-property-was-ignored).
>
> A consequence of this rule: if you list top-level variables *after* your
> service sections (the common layout used throughout this guide), the first one
> triggers exactly one such warning where it ends the last section. This is
> **benign** - the file still parses correctly. To silence it, put top-level
> variables before the sections.

## Ports

`port=` publishes a single container port:

```ini
[redis]
image=redis:7-alpine
port=6379
```

`port.<name>=` declares named ports for services that expose more than one:

```ini
[minio]
image=minio/minio
port.api=9000
port.console=9001
command=server /data --console-address ":9001"
```

Reference them as `${minio.port.api}` and `${minio.port.console}`. For a
single-port service, `${service.port}` is the one port. For a multi-port
service, always use the named form - `${service.port}` is not well-defined when
there are several ports.

## Volumes

`volume=` accepts two forms, distinguished by the first character of the host
part:

- **Named volume** - does not start with `.` or `/`. Docker manages it, and
  `eph` prefixes it per workspace (`eph-<short_id>-<service>-<name>`). Survives
  `down` and `down --rm`; removed by `clean`.

  ```ini
  volume=pgdata:/var/lib/postgresql/data
  ```

- **Bind mount** - starts with `.` or `/`. A path on your machine. Relative
  paths resolve from the workspace root. Never removed by `eph`.

  ```ini
  volume=./seed:/docker-entrypoint-initdb.d
  volume=/absolute/host/path:/data
  ```

## Health checks and timeouts

`healthcheck=` makes `eph up` wait until a command succeeds before reporting the
service as started (and before running `post-start`):

```ini
[postgres]
image=postgres:16-alpine
healthcheck=pg_isready -U dev
ready-timeout=30
```

How the command runs depends on the service type, and this matters:

- For **`image`/`dockerfile`** services, the command runs **inside the
  container** via `docker exec`, split on whitespace. It is **not** run through a
  shell - so pipes, `&&`, redirects, `$VAR` expansion, and quoted arguments
  containing spaces do **not** work. Use a single simple command (`pg_isready -U
  dev`, `redis-cli ping`).
- For **`run`** and **`compose`** services, the command runs on the **host**
  through the platform shell (`sh -c` on Unix, `cmd /C` on Windows), so full
  shell syntax is available (in that platform's shell dialect).

If you omit `healthcheck`, `eph` waits a fixed 500 ms and assumes the service is
ready. `ready-timeout` defaults to 30 seconds (60 for compose).

## Lifecycle hooks

Four hooks bracket a service's life, in this order:

| Hook | Runs | Typical use |
|------|------|-------------|
| `pre-start=` | before the service is created | codegen, a generated config the service reads |
| `post-start=` | after the service is healthy | migrations, seeding |
| `pre-stop=` | before the service stops | backup, drain |
| `post-stop=` | after the service has stopped | cleanup eph cannot do itself |

All four run on the host through the platform shell (`sh -c` on Unix, `cmd /C` on
Windows), in the workspace root, and each is repeatable and runs in order:

```ini
[api]
run=./bin/server
port=auto
# Codegen the server needs to compile, before it boots.
pre-start=go generate ./...
post-start=./scripts/seed.sh
pre-stop=./scripts/drain.sh
# Tear down a scratch bucket eph never created and cannot clean up.
post-stop=./scripts/drop-scratch-bucket.sh

[postgres]
image=postgres:16-alpine
env.POSTGRES_USER=dev
healthcheck=pg_isready -U dev
post-start=psql "$DATABASE_URL" -f schema.sql
pre-stop=./scripts/backup.sh

DATABASE_URL=postgres://dev@localhost:${postgres.port}/app
```

### Hook environment

Hooks run with eph's resolved environment injected, layered in this order (later
entries win where names collide):

1. the resolved top-level `.eph` variables -- exactly what `eph env` prints, with
   `${service.port}` filled in (so `$DATABASE_URL` above is already set);
2. `EPH_*` metadata variables:
   - `EPH_WORKSPACE_ID`, `EPH_WORKSPACE_ROOT`, `EPH_CONTAINER_PREFIX`;
   - per service `EPH_<SERVICE>_HOST`, `EPH_<SERVICE>_PORT`,
     `EPH_<SERVICE>_PORT_<NAME>` (for named ports), and `EPH_<SERVICE>_CONTAINER`.
     Service names are upper-cased with `-` replaced by `_`, so `auth-db` becomes
     `EPH_AUTH_DB_PORT`;
3. the owning service's own `env.X=` values.

The same environment is available outside hooks via [`eph run`](command-reference.md#eph-run-cmd),
which runs an arbitrary command with these variables set.

Important behavior:

- `pre-start` and `post-start` run on **every** `eph up`, for every service,
  regardless of whether the container was freshly created or an existing one was
  restarted. Write hooks to be idempotent (a migration that no-ops when applied,
  an `INSERT ... ON CONFLICT` seed); for one-off or destructive work use
  [`eph run`](command-reference.md#eph-run-cmd) instead. A failing `pre-start`
  aborts `eph up` before the service it precedes is created; a failing
  `post-start` aborts `eph up`.
- `pre-start` runs **before** its service exists, so it cannot reference that
  service's own port. It does see any service already up at that point: within a
  single `eph up`, backing services (`image`/`dockerfile`/`compose`) start before
  `run=` apps, so a `run=` app's `pre-start` can reach a database's assigned
  port. Use it for prep the service depends on, such as codegen.
- `post-start` hooks run only after **every** service in the same `eph up` is
  healthy, so a hook may reference any other service's assigned port through a
  top-level variable (for example a `DATABASE_URL` that interpolates
  `${postgres.port}`).
- A failing `pre-stop` hook aborts the `eph down` / `eph clean` and leaves the
  service running, so a backup or drain that fails is not silently skipped. Fix
  the hook and retry, or pass `--skip-hooks` to tear down without running it.
- `post-stop` runs **after** the service has stopped, for cleanup eph cannot do
  itself (deleting a scratch directory, tearing down an external resource the
  service registered). It sees the same pre-teardown environment as `pre-stop`,
  so it can still reference the now-stopped service's port. A failing `post-stop`
  aborts the rest of the teardown; because the service is already stopped, a
  later `eph down` will not re-run it, so fix the cleanup and run it by hand (or
  pass `--skip-hooks`).
- `eph up --skip-hooks` brings services up without running their `pre-start` or
  `post-start` hooks; `eph down --skip-hooks` / `eph clean --skip-hooks` tear
  down without running `pre-stop` or `post-stop`.

See [Core Concepts](concepts.md#the-service-lifecycle) for the full lifecycle.

## Roles and ordering

A `role=` tags a service with a tier, and `roles_order` orders those tiers. The
usual split is dependency services (a `dep` tier: databases, caches, queues) that
must be up before the first-party app (an `app` tier) can talk to them. Naming the
tiers lets you bring up one on its own, for example `eph up --role dep` to prewarm
the backing services without starting the app. See
[Core Concepts](concepts.md#dependency-services-vs-the-app) for the model.

```ini
roles_order=dep,app

[postgres]
image=postgres:16-alpine
role=dep
port=5432

[web]
run=npm run dev
role=app
port=auto
```

### Legacy mode vs roles mode

A file is in **legacy mode** when no service declares a `role=` and there is no
`roles_order`. Ordering is unchanged from before roles existed: services start in
declaration order with `run=` services deferred to the end, and teardown reverses
that. Existing `.eph` files need no changes.

A file is in **roles mode** the moment any service declares a `role=` or a
`roles_order` is present. Roles mode then requires all of the following, checked at
parse time (by `eph check` and before any `eph up`):

- a `roles_order` is present (linear or section form);
- every service declares a `role`;
- every service's role is listed in `roles_order`;
- every role in `roles_order` is backed by at least one service;
- every dependency edge names a known role;
- the role graph is acyclic.

A violation is a hard parse error naming the offending service or role, so a
half-specified graph never reaches `eph up`.

### `roles_order`

`roles_order` is the dependency graph over roles. "Depends on" means "must come up
first": if `app` depends on `dep`, `dep` starts before `app`, and requesting `app`
pulls `dep` in with it. Write it in one of two forms. Declaring both is an error.

**Linear form** (top-level key). A comma-separated chain where each role depends on
the one before it:

```ini
roles_order=dep,app
```

This reads "app depends on dep": `dep` comes up first, then `app`. Extend the chain
with more roles (`roles_order=dep,cache,app`) when every tier depends on the whole
tier before it.

**DAG form** (a reserved `[roles_order]` section). One `role=dep1,dep2` line per
role, spelling out each role's dependencies explicitly. A bare `role=` (empty value)
declares a root that depends on nothing. Every role must appear as a key here, roots
included:

```ini
[roles_order]
dep=
app=dep
worker=dep
```

Here both `app` and `worker` depend on `dep`, but not on each other, so a `worker`
that needs the database but not the app can start without it. Use the DAG form when
a role needs some but not all of the others; use the linear form for a straight
chain. The section may appear anywhere in the file, including before the services it
names.

### Ordering in roles mode

In roles mode the role graph is the single source of truth for order. Bring-up is
the topological order of the graph (dependencies first), with services grouped by
role and declaration order preserved within a role. Teardown is the exact reverse.

The legacy "`run=` services start last" heuristic is off in roles mode. A `run=`
service tagged as a dependency role comes up before the app that needs it, exactly
where the graph places it: the role, not the source type, decides order.

## Interpolation

Top-level environment variable values may reference running services:

```ini
[postgres]
image=postgres:16-alpine
port=5432

[minio]
image=minio/minio
port.api=9000

DATABASE_URL=postgres://localhost:${postgres.port}/db
S3_ENDPOINT=http://localhost:${minio.port.api}
HOST=${postgres.host}
```

| Syntax | Resolves to |
|--------|-------------|
| `${service.port}` | Assigned host port (single-port services) |
| `${service.port.name}` | Named port |
| `${service.host}` | `localhost` |

Interpolation is resolved by `eph env` against services that are **currently
running**. An unresolved reference (service down, or a typo'd name) is left in
place verbatim rather than blanked out.

## Complete example

```ini
# =============================================================================
# Services
# =============================================================================

[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=app
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp_dev
volume=pgdata:/var/lib/postgresql/data
healthcheck=pg_isready -U app
post-start=npm run db:migrate
post-start=npm run db:seed

[redis]
image=redis:7-alpine
port=6379
healthcheck=redis-cli ping

[minio]
image=minio/minio
port.api=9000
port.console=9001
env.MINIO_ROOT_USER=dev
env.MINIO_ROOT_PASSWORD=devdevdev
command=server /data --console-address ":9001"
volume=minio-data:/data

[mailhog]
image=mailhog/mailhog
port.smtp=1025
port.web=8025

# =============================================================================
# Environment variables
# =============================================================================

DATABASE_URL=postgres://app:dev@localhost:${postgres.port}/myapp_dev
REDIS_URL=redis://localhost:${redis.port}
S3_ENDPOINT=http://localhost:${minio.port.api}
S3_ACCESS_KEY=dev
S3_SECRET_KEY=devdevdev
SMTP_HOST=localhost
SMTP_PORT=${mailhog.port.smtp}
MAIL_WEB_UI=http://localhost:${mailhog.port.web}
APP_ENV=development
```
