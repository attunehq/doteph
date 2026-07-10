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

# A top-level variable after a service section goes in [env]
[env]
DATABASE_URL=postgres://dev@localhost:${postgres.port}/app
```

There are exactly two kinds of content:

- **Top-level environment variables**: `KEY=VALUE` lines above the first
  section, or inside an `[env]` section. These are what `eph env` prints for
  your shell.
- **Service sections**: a `[name]` header followed by `property=value` lines.
  These define what `eph up` starts.

## Syntax rules

- One directive per line.
- `key=value` splits on the **first** `=`. Both sides are trimmed of
  surrounding whitespace.
- A value may be wrapped in one matching pair of `'single'` or `"double"`
  quotes, which are stripped. The pair is only stripped when it is
  unambiguous: the value is at least two characters, starts and ends with the
  same quote, and that quote does not occur again in between. `"a" and "b"` is
  left exactly as written rather than mangled to `a" and "b`. Quotes are only
  needed to preserve leading or trailing spaces; they are otherwise optional.
- A leading UTF-8 byte-order mark (some Windows editors add one automatically)
  is ignored.
- Within a section, property order does not matter, and interpolation refers
  to services by name, so a service can be referenced from anywhere in the
  file. **Sections do not end at blank lines or comments**: once a
  `[section]` opens, every following line belongs to it until the next
  `[section]`, `[env]`, or `[roles_order]` header. A bare top-level variable
  therefore cannot follow a service section directly; see
  [Where to put top-level variables](#where-to-put-top-level-variables).

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

A top-level environment variable is a bare `KEY=VALUE` line, written either
above the first section or inside an `[env]` section:

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

A top-level variable name must be a valid shell identifier:
`^[A-Za-z_][A-Za-z0-9_]*$` (letters, digits, and underscores, not starting
with a digit). Anything else would break the `export NAME=...` line `eph env`
emits. The top-of-file block and every `[env]` section share one namespace, so
declaring the same name twice anywhere is a duplicate-variable error, even
across the two forms.

### Where to put top-level variables

Sections do not end at blank lines, so once you are inside `[postgres]`, a
bare `KEY=VALUE` line is ambiguous: is it a new service property, or a
variable meant for your shell? The parser does not guess. A bare top-level
variable is legal in exactly two places:

- **Above the first section.** Nothing has opened yet, so every `KEY=VALUE`
  line is a top-level variable.
- **Inside an `[env]` section.** `[env]` is a reserved section name, never a
  service: every line inside it is a top-level variable, exactly like the
  top-of-file block. `[env]` may appear more than once; each occurrence
  switches back into variable context, so you can group a service's variables
  near its own section:

  ```ini
  [postgres]
  image=postgres:16-alpine
  port=5432

  [env]
  DATABASE_URL=postgres://dev@localhost:${postgres.port}/app

  [redis]
  image=redis:7-alpine
  port=6379

  [env]
  REDIS_URL=redis://localhost:${redis.port}
  ```

A bare `KEY=VALUE` written directly after a service section, with no `[env]`,
is a **hard parse error**:

```
'DATABASE_URL' at line 5 looks like an environment variable, but it is inside
service 'postgres' (sections do not end at blank lines). To set it in the
container, write env.DATABASE_URL=...; to export it from `eph env`, move it
into an [env] section or above the first section
```

The conventional layout is variables first, then services, then a trailing
`[env]` section for anything that needs to reference a service:

```ini
APP_ENV=development

[postgres]
image=postgres:16-alpine
port=5432

[env]
DATABASE_URL=postgres://dev@localhost:${postgres.port}/app
```

A misspelled `[env]` (`[envs]`, `[vars]`, `[variables]`, `[environment]`) is
rejected with a hint pointing at `[env]` rather than being treated as an
unknown, unclassified service.

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

Declare exactly one source. A section that declares a second one, whether the
same key twice (`image=` twice) or two different keys (`image=` and `run=`),
is a parse error naming the service and line. The four sources are covered in
depth in [Defining Services](services.md).

Service names must match `^[a-z][a-z0-9-]*$`: lowercase letters, digits, and
hyphens, starting with a letter. The rule is strict because a service name
becomes three other things: a container name, the `service` half of a
`${service.property}` reference (a `.` would split at the wrong place), and
the `EPH_<NAME>_*` metadata variables (allowing both `-` and `_` would let
`auth-db` and `auth_db` collide once both are upper-cased). `[My-Service]`,
`[auth_db]`, and `[1db]` are all rejected.

Reopening a section name later in the file (`[db]` ... `[db]` again) is a
parse error naming both line numbers; sections are never silently merged.

Environment variable names must match `^[A-Za-z_][A-Za-z0-9_]*$`. Names that
start with `EPH_`, in any letter case, are reserved for eph's workspace and
service metadata and are rejected in top-level variables, `[env]`, and
`env.<KEY>=` properties.

### Service properties

| Property | Repeatable | Description |
|----------|:----------:|-------------|
| `image=` | no | Docker image to pull and run. |
| `dockerfile=` | no | Path to a Dockerfile to build (relative to the workspace). |
| `context=` | no | Build context for `dockerfile=` (defaults to the Dockerfile's directory). Illegal with every other source. |
| `compose=` | no | Path to a Docker Compose file to delegate to. |
| `run=` | no | Shell command for a non-Docker service. |
| `role=` | no | The role (tier) this service belongs to; a free-form name you choose, such as `dep` or `app`. See [Roles and ordering](#roles-and-ordering). |
| `command=` | no | Override the container's default command. Only legal for `image=`/`dockerfile=` services: a `run=` service's command *is* its `run=` value, and a compose service's command lives in the compose file, so `command=` there is a parse error. |
| `port=` | no (one per service) | A container port to publish on a random host port. For `run=` services, `port=auto` makes eph allocate the port (see [Running Your App](run-your-app.md#portauto)). Illegal on `compose=` services; use `expose.<name>=` there instead. |
| `port.<name>=` | one per distinct name | A **named** port, for multi-port services. `port.<name>=auto` is allowed for `run=` services. Same restrictions as `port=`. |
| `env.<KEY>=` | one per distinct key | An environment variable passed into the container. |
| `volume=` | yes | A volume mount: `name:/path` (named) or `./host:/path` (bind). Only legal for `image=` and `dockerfile=` services. |
| `healthcheck=` | no | Command that must succeed before the service counts as ready. |
| `ready-timeout=` | no | Non-zero seconds to wait for the `healthcheck` (default 30; 60 for compose). Requires `healthcheck=`. |
| `pre-start=` | yes | Hook run before the service is created. |
| `post-start=` | yes | Hook run after every service in the `up` is healthy. |
| `pre-stop=` | yes | Hook run before the service is stopped. |
| `post-stop=` | yes | Hook run after the service has stopped. |
| `expose.<alias>=` | one per distinct alias | For `compose=`: expose `<compose-service>:<container-port>` for interpolation. The short form `<container-port>` targets the Compose service named by the alias. Illegal on every other source. |

"Repeatable" means the property can appear multiple times and every value is
kept; several `post-start=` lines run in order. Every non-repeatable property
is single-valued: a second occurrence (even of a differently-named
`port.<name>=` that repeats an already-used name) is a parse error naming the
property, service, and line. Hooks and `volume=` are the only properties
designed to repeat and accumulate.

An unknown property, of any case, is a parse error listing every known
property name. There is no reclassification: a stray `HEALTHCHECK=...` inside
a section is rejected with a hint to write `env.HEALTHCHECK=...` (to set it in
the container) or move it into `[env]` (to export it from your shell), the
same distinction as the [top-level-variable rule](#where-to-put-top-level-variables)
above.

Every property except `env.<KEY>=` rejects an empty value: `image=`,
`volume=`, `healthcheck=`, `post-start=`, and the rest are parse errors with
nothing after the `=`. `env.<KEY>=` alone stays legal, since setting a
container variable to the empty string is a real thing to want.

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

Port names follow the same rule as service names (`^[a-z][a-z0-9-]*$`): they
become part of `${service.port.<name>}` interpolation and the
`EPH_<SERVICE>_PORT_<NAME>` metadata variable.

Reference them as `${minio.port.api}` and `${minio.port.console}`. A service
with exactly one port may use `${service.port}`, whether that port was declared
as `port=` or `port.<name>=`. A bare `${service.port}` is a parse error when the
service exposes zero or multiple ports; use a named reference for multi-port
services.

`port=` and `port.<name>=` are only for services eph itself publishes:
`image=`, `dockerfile=`, and `run=`. On a `compose=` service they are a parse
error; declare `expose.<name>=` instead (see
[Defining Services](services.md#compose-delegate-to-docker-compose)).

## Volumes

`volume=` accepts two forms, distinguished by the shape of the host part:

Volumes are supported only for `image=` and `dockerfile=` services. Compose
volumes belong in the Compose file, and host processes use ordinary paths.

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
`ready-timeout=0` and a timeout without a health check are parse errors.

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

[env]
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
top-level variables outside the section. A misspelled section name
(`[role_order]`, `[roles-order]`, `[roles]`, and similar) is rejected with a
hint pointing at `[roles_order]`.

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

Top-level environment variable values, and a service's `env.<KEY>=` values,
may reference other services:

```ini
[postgres]
image=postgres:16-alpine
port=5432

[minio]
image=minio/minio
port.api=9000

[env]
DATABASE_URL=postgres://localhost:${postgres.port}/db
S3_ENDPOINT=http://localhost:${minio.port.api}
HOST=${postgres.host}
```

| Syntax | Resolves to |
|--------|-------------|
| `${service.port}` | Assigned host port (single-port services) |
| `${service.port.name}` | Named port |
| `${service.host}` | `localhost` |

Two different things happen at two different times:

- **At parse time**, `eph check` (and every other command) validates the
  *shape* of every placeholder in a top-level variable or `env.<KEY>=` value:
  an unterminated `${` is an error, a placeholder that is not the two-part
  `${service.property}` form is an error, and a placeholder naming a service
  that is not defined anywhere in the file is an error. A service defined
  later in the file is fine; the check runs after the whole file is read, so
  forward references work. A literal `${` that is not meant as a placeholder
  is written `$${`, which renders as `${` and is never validated as a
  reference:

  ```ini
  [env]
  TEMPLATE=cost: $${not.a.placeholder}
  ```

- **At runtime**, `eph env`, hooks, `eph run`, and every service's own
  `env.<KEY>=` values resolve each placeholder against **currently running**
  services. This is consistent across every source: an `image=` or
  `dockerfile=` service's `env.<KEY>=` is resolved just before its container is
  created, a `compose=` service's `env.<KEY>=` is resolved into the environment
  `docker compose up` and port discovery run with (so the compose file's own
  `${VAR}` substitution sees it too), and a `run=` service's
  `env.<KEY>=` is resolved into the process it launches. For hooks, `eph run`,
  and every service's own `env.<KEY>=`, every reference must resolve before
  eph launches the hook, command, process, container, or Compose invocation.
  A stopped dependency is an error naming the affected variable and reference.

  Shell output from `eph env` has one extra safety requirement: stale variables
  from an earlier successful evaluation must be cleared. It emits an unset for
  each unresolved variable, appends a shell failure statement, warns on stderr,
  and exits non-zero. JSON output omits unresolved variables and also exits
  non-zero. For example:

  ```
  warning: DATABASE_URL: ${postgres.port} is not resolvable while postgres is not running
  ```

  Run `eph up` and evaluate `eph env` again once the dependency is running.

  Resolved values are host-facing: `${service.port}` is the port Docker
  published on the host's loopback interface, and `${service.host}` is always
  `localhost` as seen **from the host**. That is exactly right for a hook,
  `eph run`, or a `run=` process, all of which execute on the host. It is
  usually the wrong address for one container reaching another: a `postgres`
  container's `env.DATABASE_URL=...${redis.port}` receives a literal
  `localhost:PORT`, but inside that container `localhost` means the container
  itself, not the host, so it will not reach `redis`. Reach a sibling service
  from inside a container through `host.docker.internal` (Docker Desktop) or by
  addressing the sibling on a shared Docker network by its container name and
  **container** port, not through eph's interpolated, host-facing value.

`run=` and hook (`pre-start`/`post-start`/`pre-stop`/`post-stop`) command
strings are shell commands, so `${VAR}` there belongs to the shell rather than
eph. Health checks preserve ordinary shell forms such as `${PORT}` too, while
recognizing, validating, and resolving dotted eph references such as
`${api.port}` before execution.

For a `compose` service, `expose.<alias>=<compose-service>:<container-port>`
resolves as `${service.port.<alias>}`. The short form
`expose.<alias>=<container-port>` targets the Compose service whose name is the
alias. Failure to query that exact mapping from Compose is a startup error.

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

[env]
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
