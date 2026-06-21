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
| `image=` | no | Docker image to pull and run. |
| `dockerfile=` | no | Path to a Dockerfile to build (relative to workspace). |
| `context=` | no | Build context for `dockerfile=` (defaults to the Dockerfile's directory). |
| `compose=` | no | Path to a Docker Compose file to delegate to. |
| `run=` | no | Shell command for a non-Docker service. |
| `command=` | no | Override the container's default command (`image`/`dockerfile` only). |
| `port=` | yes | A container port to publish on a random host port. |
| `port.<name>=` | yes | A **named** container port (for multi-port services). |
| `env.<KEY>=` | yes | An environment variable passed into the container. |
| `volume=` | yes | A volume mount: `name:/path` (named) or `./host:/path` (bind). |
| `healthcheck=` | no | Command that must succeed before the service is "ready". |
| `ready-timeout=` | no | Seconds to wait for the `healthcheck` (default 30; 60 for compose). Ignored when no `healthcheck` is set. |
| `post-start=` | yes | Command run after the service becomes healthy. |
| `pre-stop=` | yes | Command run before the service is stopped. |
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
  through `sh -c`, so full shell syntax is available.

If you omit `healthcheck`, `eph` waits a fixed 500 ms and assumes the service is
ready. `ready-timeout` defaults to 30 seconds (60 for compose).

## Lifecycle hooks

`post-start=` runs after a service is healthy; `pre-stop=` runs before it stops.
Both run on the host through `sh -c`, in the workspace root, and both are
repeatable and run in order:

```ini
[postgres]
image=postgres:16-alpine
healthcheck=pg_isready -U dev
post-start=npm run db:migrate
post-start=npm run db:seed
pre-stop=./scripts/backup.sh
```

Important behavior:

- For `image`/`dockerfile` services, `post-start` runs only when a container is
  **created fresh**, not when a stopped container is restarted by a later `eph
  up`. For `run` services it runs whenever the process is not already alive, and
  for `compose` services it runs on **every** `eph up`. (See
  [Core Concepts](concepts.md#the-service-lifecycle).) A failing `post-start`
  aborts `eph up`.
- `pre-stop` failures are logged but do not stop the teardown.

See [Core Concepts](concepts.md#the-service-lifecycle) for exactly when each
path is taken.

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
