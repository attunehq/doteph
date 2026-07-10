---
title: "Defining Services"
summary: "The four service sources, with ready-to-use definitions for common services."
order: 4
---

# Defining Services

Every service declares a **source**: the thing `eph` starts. There are four:

| Source | Use it for |
|--------|-----------|
| `image=` | An existing Docker image (the common case). |
| `dockerfile=` | A custom image you build from a local Dockerfile. |
| `compose=` | A multi-container subsystem you already maintain as Compose. |
| `run=` | A plain process on the host: your own app, a local binary. |

Declare exactly one per service. A section that declares a second one (the
same key twice, or two different keys) is a parse error. This page covers the
first three in depth and introduces `run=`; [Running Your App](run-your-app.md)
is the full story for first-party apps. The
[common definitions](#common-service-definitions) at the end are ready to
paste.

## `image=`: Docker image services

The common case: pull and run an existing image.

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=app_dev
volume=pgdata:/var/lib/postgresql/data
healthcheck=pg_isready -U dev
```

`eph` pulls the image if it is not present, creates a container named
`eph-<short_id>-postgres`, publishes each `port=` on a random host port bound
to loopback, applies your `env.*` and `volume=` settings, and waits for the
`healthcheck`.

An `env.<KEY>=` value can reference another service with
`${service.property}`, resolved against whichever services are already
running at the moment this container is created, the same interpolation as a
top-level `[env]` variable. See
[Interpolation](eph-file.md#interpolation) for the full contract, including
why the resolved value (a host-facing `localhost:PORT`) usually is not the
right address for one container to reach another.

Use `command=` to override the image's default command:

```ini
[minio]
image=minio/minio
port.api=9000
port.console=9001
command=server /data --console-address ":9001"
```

`command=` is parsed with shell-style word splitting (quotes are respected),
but it is **not** run through a shell; it replaces the container's argv
directly.

## `dockerfile=`: build a local image

When you need a custom image, build it from a Dockerfile. Paths are relative
to the workspace root.

```ini
[worker]
dockerfile=./docker/worker.Dockerfile
context=.
port=8080
env.WORKER_THREADS=4
```

- `context=` is the build context; if omitted, it defaults to the directory
  containing the Dockerfile.
- The built image is tagged `eph-<short_id>-worker` and cached, so subsequent
  `eph up` runs are fast.
- After building, the service behaves exactly like an `image=` service: ports,
  env, volumes, health check, hooks.

> Building shells out to the `docker` CLI, so `docker build` must work in your
> environment.

## `compose=`: delegate to Docker Compose

For multi-container subsystems you already maintain as Compose (Kafka plus
Zookeeper, an observability stack), delegate to the Compose file rather than
translating it:

```ini
[kafka]
compose=./docker/kafka-compose.yml
expose.kafka=9092
expose.zookeeper=2181
```

- `eph` runs `docker compose -f <file> -p eph-<short_id>-kafka up -d`, so the
  whole project is namespaced per workspace.
- `expose.<name>=<container_port>` makes a port available for interpolation as
  `${kafka.port.kafka}` and so on. `eph` asks `docker compose port` for the
  real mapped host port and falls back to the declared value if Compose does
  not report one (with a warning, since the declared port is usually not the
  actual mapped one).
- `env.<KEY>=`, with `${service.property}` references resolved against
  running services first, is exported into the process environment `docker
  compose up` and `docker compose down` themselves run with, so your compose
  file's own `${VAR}` substitution can read it.
- Compose services are tracked by `eph status` and `eph env`. Compose names its
  own containers, so `eph` finds the project by its
  `com.docker.compose.project` label rather than by container name.
- `ready-timeout` defaults to **60 seconds** for compose services.

> Requires the `docker compose` CLI plugin.

### How compose services differ

Compose support is intentionally thin: `eph` shells out to `docker compose`
and lets it own the container lifecycle. Three differences from the other
sources are worth knowing:

- **Teardown is coarser.** Both `eph down` and `eph down --rm` run
  `docker compose ... down`, which removes the compose containers either way.
  `--rm` makes no difference for compose.
- **`eph clean` does not remove Compose-internal volumes.** It removes only
  the named volumes you declare with `volume=` in the `.eph` file. Volumes
  defined inside the Compose file belong to `docker compose`; run
  `docker compose ... down -v` yourself if you need to drop them.
- **A failed `docker compose down` is a real error.** If the compose file is
  broken or the `docker compose` plugin is missing, `eph down` and
  `eph clean` stop and report it rather than treating it as success; fix the
  underlying problem and re-run.

## `run=`: a process instead of a container

For services that are not containers (a locally installed binary, LocalStack,
and above all **your own app**):

```ini
[localstack]
run=localstack start
port=4566
env.SERVICES=s3,sqs,dynamodb
healthcheck=curl -sf http://localhost:4566/_localstack/health
```

The essentials:

- The command runs through the platform shell (`sh -c` on Unix, `cmd /C` on
  Windows) in the workspace root, and its process is tracked in state.
- Because eph launches it, the process inherits eph's **resolved** environment:
  the variables `eph env` emits (like `DATABASE_URL`), the `EPH_*` metadata,
  and the service's own `env.*` values with `${...}` resolved. A managed app
  reaches the rest of the workspace without any `eval`.
- **Fixed ports are not remapped.** With a numeric `port=`, the process binds
  whatever it binds, and `eph` reports the declared value as-is for
  interpolation. Declare the port your process will actually use, or use
  `port=auto` to have eph allocate one and inject it.
- The `healthcheck` (if any) runs on the host through the platform shell with
  the same resolved environment the process gets, so a readiness check can
  reach an auto-allocated port: `curl -sf http://localhost:$PORT/health`.
- `eph down` stops the process gracefully, waits, then force-kills, and it
  targets the **whole process tree** the command spawned, so a compound
  command (`run=build && serve`, a pipeline, a backgrounded child) is torn
  down completely. Starting an already-running `run` service again is a no-op.

`run=` services work natively on Linux, macOS, and Windows. On Windows the
command goes through `cmd`, so a command string written for `sh` may need a
`cmd`-compatible form, or run eph inside WSL to keep POSIX commands. See
[Troubleshooting](troubleshooting.md#windows).

This source is how you put the app you are building under eph's management:
`port=auto`, restart self-healing, `eph dev`, and preview servers are all
covered in [Running Your App](run-your-app.md).

## Multi-port services

A service can expose several named ports. Reference them by name:

```ini
[minio]
image=minio/minio
port.api=9000
port.console=9001

[env]
S3_ENDPOINT=http://localhost:${minio.port.api}
S3_CONSOLE=http://localhost:${minio.port.console}
```

For a single-port service use `${service.port}`. For multi-port services
always use the named form.

## Common service definitions

Copy these into your `.eph` file and adjust credentials and versions.

### PostgreSQL

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
volume=pgdata:/var/lib/postgresql/data
healthcheck=pg_isready -U dev

[env]
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
```

### MySQL / MariaDB

```ini
[mysql]
image=mysql:8
port=3306
env.MYSQL_ROOT_PASSWORD=dev
env.MYSQL_DATABASE=myapp
env.MYSQL_USER=dev
env.MYSQL_PASSWORD=dev
volume=mysqldata:/var/lib/mysql
healthcheck=mysqladmin ping -h localhost

[env]
DATABASE_URL=mysql://dev:dev@localhost:${mysql.port}/myapp
```

### Redis

```ini
[redis]
image=redis:7-alpine
port=6379
healthcheck=redis-cli ping

[env]
REDIS_URL=redis://localhost:${redis.port}
```

### MongoDB

```ini
[mongo]
image=mongo:7
port=27017
env.MONGO_INITDB_ROOT_USERNAME=dev
env.MONGO_INITDB_ROOT_PASSWORD=dev
volume=mongodata:/data/db
healthcheck=mongosh --eval db.adminCommand(ping)

[env]
MONGO_URL=mongodb://dev:dev@localhost:${mongo.port}
```

> Image health checks run without a shell, so keep them to one command with no
> quoted spaces. `mongosh --eval db.adminCommand(ping)` works because it is
> plain whitespace-separated arguments.

### MinIO (S3-compatible)

```ini
[minio]
image=minio/minio
port.api=9000
port.console=9001
env.MINIO_ROOT_USER=dev
env.MINIO_ROOT_PASSWORD=devdevdev
command=server /data --console-address ":9001"
volume=miniodata:/data

[env]
S3_ENDPOINT=http://localhost:${minio.port.api}
S3_ACCESS_KEY=dev
S3_SECRET_KEY=devdevdev
```

### MailHog (catch-all SMTP with a web UI)

```ini
[mailhog]
image=mailhog/mailhog
port.smtp=1025
port.web=8025

[env]
SMTP_HOST=localhost
SMTP_PORT=${mailhog.port.smtp}
MAIL_WEB_UI=http://localhost:${mailhog.port.web}
```

### RabbitMQ

```ini
[rabbitmq]
image=rabbitmq:3-management
port.amqp=5672
port.ui=15672
env.RABBITMQ_DEFAULT_USER=dev
env.RABBITMQ_DEFAULT_PASS=dev
healthcheck=rabbitmq-diagnostics -q ping

[env]
AMQP_URL=amqp://dev:dev@localhost:${rabbitmq.port.amqp}
```

## Next

Your backing services are defined. [Running Your App](run-your-app.md) brings
the app you are actually building into the same workspace.
