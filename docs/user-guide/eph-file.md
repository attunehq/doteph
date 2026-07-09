---
title: "The .eph File"
summary: "The complete file format: variables, services, every property, roles, and interpolation."
order: 3
---

# The `.eph` File

The `.eph` file is the entire configuration for a workspace. It extends `.env`
syntax with INI-style `[sections]` for services; a plain `.env` file is already
a valid `.eph` file, and you add services on top. This page is the complete
format. If you have not read [Core Concepts](concepts.md), start there: this
page tells you *what* you can write, that one tells you what it means.

## Anatomy

```ini
# A top-level environment variable (exported by `eph env`)
APP_ENV=development

# A service definition
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev

# Top-level variables can reference services
DATABASE_URL=postgres://dev@localhost:${postgres.port}/app
```

There are exactly two kinds of content:

- **Top-level environment variables**: `KEY=VALUE` lines outside any section.
  These are what `eph env` prints for your shell.
- **Service sections**: a `[name]` header followed by `property=value` lines.
  These define what `eph up` starts.

## Syntax rules

- One directive per line.
- `key=value` splits on the **first** `=`. Both sides are trimmed of
  surrounding whitespace.
- A value may be wrapped in one matching pair of `'single'` or `"double"`
  quotes, which are stripped. Quotes are only needed to preserve leading or
  trailing spaces; they are otherwise optional.
- Within a section, property order does not matter, and interpolation refers to
  services by name, so a service can be referenced from anywhere in the file.
  Placement of top-level variables does matter, though: a top-level variable
  written after a `[section]` ends that section (see
  [the reclassification rule](#where-to-put-top-level-variables)). Blank lines
  and comments do **not** end a section.

### Comments

A comment is a line whose first non-whitespace character is `#`:

```ini
# This is a comment.
image=postgres:16-alpine
```

**Comments must be on their own line.** There are no inline or trailing
comments; a `#` after a value becomes part of the value:

```ini
port=5432            # WRONG: the value is "5432            # WRONG..." and
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

These are the variables `eph env` emits for your shell to load. They may
contain `${service.property}` references (see [Interpolation](#interpolation)).

> Do not confuse these with `env.KEY=` *inside* a service section. Top-level
> variables go to **your shell**; `env.KEY=` goes **into the container**. They
> are separate namespaces.

### Where to put top-level variables

An unknown lowercase key inside a section is a hard parse error (a typo like
`prot=5432` cannot slip through). An unknown `SCREAMING_SNAKE_CASE` key inside
a section is treated differently: it is reclassified as a top-level environment
variable, it **ends the section**, and `eph` prints a warning. That rule is
what lets you write top-level variables after your services, but it has one
sharp edge: a miscased property such as `HEALTHCHECK=...` silently becomes a
global variable instead of a health check. See
[Troubleshooting](troubleshooting.md#a-property-was-ignored).

Both conventional layouts work: variables first, then sections; or sections
first, then a trailing block of variables. With the trailing layout, the first
variable after the last section triggers exactly one reclassification warning.
That warning is benign, and putting the variables before the sections silences
it.

## Service sections

A service is a `[bracketed]` section followed by its properties:

```ini
[redis]
image=redis:7-alpine
port=6379
healthcheck=redis-cli ping
```

Every service **must declare a source**: `image`, `dockerfile`, `compose`, or
`run`. A section with no source is rejected at parse time, by `eph check` and
before any `eph up`:

```
service 'redis' has no source defined (set one of image/dockerfile/compose/run)
```

Declare exactly one source. If a section lists several, the last one silently
wins; this is not validated, so treat it as a mistake to avoid. The four
sources are covered in depth in [Defining Services](services.md).

### Service properties

| Property | Repeatable | Description |
|----------|:----------:|-------------|
| `image=` | no | Docker image to pull and run. |
| `dockerfile=` | no | Path to a Dockerfile to build (relative to the workspace). |
| `context=` | no | Build context for `dockerfile=` (defaults to the Dockerfile's directory). |
| `compose=` | no | Path to a Docker Compose file to delegate to. |
| `run=` | no | Shell command for a non-Docker service. |
| `role=` | no | The role (tier) this service belongs to; a free-form name you choose, such as `dep` or `app`. See [Roles and ordering](#roles-and-ordering). |
| `command=` | no | Override the container's default command (`image` and `dockerfile` only). |
| `port=` | yes | A container port to publish on a random host port. For `run=` services, `port=auto` makes eph allocate the port (see [Running Your App](run-your-app.md#portauto)). |
| `port.<name>=` | yes | A **named** port, for multi-port services. `port.<name>=auto` is allowed for `run=` services. |
| `env.<KEY>=` | yes | An environment variable passed into the container. |
| `volume=` | yes | A volume mount: `name:/path` (named) or `./host:/path` (bind). |
| `healthcheck=` | no | Command that must succeed before the service counts as ready. |
| `ready-timeout=` | no | Seconds to wait for the `healthcheck` (default 30; 60 for compose). Ignored when no `healthcheck` is set. |
| `pre-start=` | yes | Hook run before the service is created. |
| `post-start=` | yes | Hook run after every service in the `up` is healthy. |
| `pre-stop=` | yes | Hook run before the service is stopped. |
| `post-stop=` | yes | Hook run after the service has stopped. |
| `expose.<name>=` | yes | For `compose=`: expose a port for interpolation. |

"Repeatable" means the property can appear multiple times and every value is
kept; several `post-start=` lines run in order.

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
service, always use the named form; `${service.port}` is not well-defined when
there are several.

## Volumes

`volume=` accepts two forms, distinguished by the shape of the host part:

- **Named volume**: a bare name that does not look like a path. Docker manages
  it, and `eph` prefixes it per workspace (`eph-<short_id>-<service>-<name>`).
  It survives `down` and `down --rm` and is removed by `clean`.

  ```ini
  volume=pgdata:/var/lib/postgresql/data
  ```

- **Bind mount**: a path on your machine. Relative paths (starting with `.`)
  resolve from the workspace root. `eph` never deletes bind mounts. The host
  part counts as a path when it starts with `.` or `/`, or, on Windows, when it
  is a drive-letter path (`C:\...` or `C:/...`) or a UNC path
  (`\\server\share\...`).

  ```ini
  volume=./seed:/docker-entrypoint-initdb.d
  volume=/absolute/host/path:/data
  volume=C:\Users\me\data:/data
  ```

## Health checks and timeouts

`healthcheck=` makes `eph up` wait until a command succeeds before reporting
the service as started (and before any `post-start` hooks run):

```ini
[postgres]
image=postgres:16-alpine
healthcheck=pg_isready -U dev
ready-timeout=30
```

Where the command runs depends on the service type, and this matters:

- For **`image` and `dockerfile`** services, the command runs **inside the
  container** via `docker exec`, split on whitespace. It is **not** run through
  a shell, so pipes, `&&`, redirects, `$VAR` expansion, and quoted arguments
  containing spaces do **not** work. Use one simple command:
  `pg_isready -U dev`, `redis-cli ping`.
- For **`run`** and **`compose`** services, the command runs on the **host**
  through the platform shell (`sh -c` on Unix, `cmd /C` on Windows), so full
  shell syntax is available in that platform's dialect.

If you omit `healthcheck`, `eph` waits a fixed 500 ms and assumes the service
is ready. `ready-timeout` defaults to 30 seconds, or 60 for compose services.

## Lifecycle hooks

Four hooks bracket a service's life. This section is the authoritative
reference for how they behave.

| Hook | Runs | Typical use |
|------|------|-------------|
| `pre-start=` | before the service is created | codegen, a generated config the service reads |
| `post-start=` | after every service in the `up` is healthy | migrations, seeding |
| `pre-stop=` | before the service stops | backup, drain |
| `post-stop=` | after the service has stopped | cleanup eph cannot do itself |

All four run on the host through the platform shell (`sh -c` on Unix, `cmd /C`
on Windows), in the workspace root. Each is repeatable, and repeated hooks run
in order:

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

Hooks run with eph's resolved environment injected, layered in this order
(later entries win where names collide):

1. The resolved top-level `.eph` variables: exactly what `eph env` prints, with
   `${service.port}` filled in. `$DATABASE_URL` in the example above is already
   set; no `eval` needed.
2. `EPH_*` metadata variables: `EPH_WORKSPACE_ID`, `EPH_WORKSPACE_ROOT`, and
   `EPH_CONTAINER_PREFIX`, plus per service `EPH_<SERVICE>_HOST`,
   `EPH_<SERVICE>_PORT`, `EPH_<SERVICE>_PORT_<NAME>` (for named ports), and
   `EPH_<SERVICE>_CONTAINER`. Service names are upper-cased with `-` replaced
   by `_`, so `auth-db` becomes `EPH_AUTH_DB_PORT`.
3. The owning service's own `env.X=` values.

The same environment is available outside hooks via
[`eph run`](command-reference.md#eph-run-cmd), which runs an arbitrary command
with these variables set.

### Startup hooks: `pre-start` and `post-start`

- **Both run on every `eph up`**, for every service, whether its container was
  freshly created or an existing one was restarted. Write them to be
  idempotent: a migration that no-ops when applied, an
  `INSERT ... ON CONFLICT` seed. For one-off or destructive work, use
  [`eph run`](command-reference.md#eph-run-cmd) instead of a hook.
- **`pre-start` runs before its service exists**, so it cannot reference that
  service's own port. It does see any service already up at that point: within
  a single `eph up`, backing services start before `run=` apps (or in role
  order), so an app's `pre-start` can reach the database's assigned port.
- **`post-start` hooks run in a second phase**, only after **every** service in
  the `up` is healthy. A `post-start` hook can therefore reference any
  service's assigned port, for example a migration whose `DATABASE_URL`
  interpolates `${postgres.port}`.
- **A failure aborts the `up`.** A failing `pre-start` aborts before its
  service starts; a failing `post-start` aborts the `up` at that point.

### Teardown hooks: `pre-stop` and `post-stop`

- **A failing `pre-stop` leaves the service running** and aborts the `down` or
  `clean`, so a backup or drain that fails is never silently skipped. Fix the
  hook and retry.
- **`post-stop` runs after the service has stopped**, for cleanup eph cannot do
  itself. It sees the same pre-teardown environment as `pre-stop`, so it can
  still reference the now-stopped service's port. A failing `post-stop` aborts
  the rest of the teardown, but its own service is already stopped, so a later
  `eph down` will not re-run it; fix the cleanup and run it by hand.

### Skipping hooks

`--skip-hooks` on `eph up` skips `pre-start` and `post-start`; on `eph down`
and `eph clean` it skips `pre-stop` and `post-stop`. It is the escape hatch for
a broken hook that is wedging startup or teardown.

## Roles and ordering

A `role=` tags a service with a tier, and `roles_order` declares the dependency
graph over those tiers. The usual split is a `dep` tier of backing services
(databases, caches, queues) that must be up before the `app` tier can talk to
them. Naming the tiers also makes them addressable: `eph up --role dep`
prewarms the backing services without starting the app. The model is in
[Core Concepts](concepts.md#dependency-services-vs-the-app).

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
`roles_order`. Services start in declaration order with `run=` services
deferred to the end, and teardown reverses that. Existing `.eph` files need no
changes.

A file is in **roles mode** the moment any service declares a `role=` or a
`roles_order` appears. Roles mode then requires all of the following, checked
at parse time (by `eph check` and before any `eph up`):

- a `roles_order` is present (linear or DAG form);
- every service declares a `role`;
- every service's role is listed in `roles_order`;
- every role in `roles_order` is backed by at least one service;
- every dependency edge names a known role;
- the role graph is acyclic.

A violation is a hard parse error naming the offending service or role, so a
half-specified graph never reaches `eph up`.

### `roles_order`

"Depends on" means "must come up first": if `app` depends on `dep`, then `dep`
starts before `app`, and requesting `app` pulls `dep` in with it. Write the
graph in one of two forms; declaring both is an error.

**Linear form** (a top-level key): a comma-separated chain where each role
depends on the one before it.

```ini
roles_order=dep,app
```

This reads "app depends on dep": `dep` comes up first, then `app`. Extend the
chain (`roles_order=dep,cache,app`) when every tier depends on the whole tier
before it.

**DAG form** (a reserved `[roles_order]` section): one `role=dep1,dep2` line
per role, spelling out each role's dependencies explicitly. A bare `role=`
(empty value) declares a root that depends on nothing. Every role must appear
as a key, roots included:

```ini
[roles_order]
dep=
app=dep
worker=dep
```

Here `app` and `worker` both depend on `dep` but not on each other, so a
`worker` that needs the database but not the app can start without it. Use the
DAG form when a role needs some but not all of the others; use the linear form
for a straight chain.

The `[roles_order]` section may appear anywhere in the file, including before
the services it names. Every line inside it is a role edge (role names are
free-form, so nothing is reinterpreted as an environment variable), so keep
top-level variables outside the section.

### Ordering in roles mode

In roles mode the role graph is the single source of truth for order. Bring-up
follows the topological order of the graph (dependencies first), with services
grouped by role and declaration order preserved within a role. Teardown is the
exact reverse.

The legacy "`run=` services start last" heuristic is off in roles mode. A
`run=` service tagged as a dependency role comes up before the app that needs
it, exactly where the graph places it. The role, not the source type, decides
order.

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

Interpolation is resolved by `eph env` (and by hooks, `eph run`, and `run=`
service environments) against services that are **currently running**. An
unresolved reference (a stopped service, or a typo'd name) is left in place
verbatim rather than blanked out, so mistakes stay visible.

For a `compose` service, the ports you declared with `expose.<name>=` resolve
as `${service.port.<name>}`.

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

A larger annotated example, including roles and a `run=` app, ships in the
repository as
[`example.eph`](https://github.com/attunehq/doteph/blob/main/example.eph).

## Next

[Defining Services](services.md) goes deep on the four service sources and
gives ready-to-use definitions for common services.
