# Defining Services

Every service declares a **source** - the thing `eph` starts. There are four:
`image`, `dockerfile`, `compose`, and `run`. Declare exactly one per service
(if several are listed, the last wins - so treat that as a mistake). This page
covers each, then gives ready-to-use definitions for common services.

## `image=` - Docker image services

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
`eph-<short_id>-postgres`, publishes each `port=` on a random host port bound to
loopback, applies your `env.*` and `volume=` settings, and waits for the
`healthcheck`.

Use `command=` to override the image's default command:

```ini
[minio]
image=minio/minio
port.api=9000
port.console=9001
command=server /data --console-address ":9001"
```

`command=` is parsed with shell-style word splitting (quotes are respected), but
it is **not** run through a shell - it replaces the container's argv directly.

## `dockerfile=` - build a local image

When you need a custom image, build it from a Dockerfile. Paths are relative to
the workspace root.

```ini
[worker]
dockerfile=./docker/worker.Dockerfile
context=.
port=8080
env.WORKER_THREADS=4
```

- `context=` is the build context. If omitted, it defaults to the directory
  containing the Dockerfile.
- The built image is tagged `eph-<short_id>-worker` and cached, so subsequent
  `eph up` runs are fast.
- After building, the service behaves exactly like an `image=` service (ports,
  env, volumes, health check, hooks).

> Building shells out to the `docker` CLI, so `docker build` must work in your
> environment.

## `compose=` - delegate to Docker Compose

For multi-container subsystems you already maintain as Compose (Kafka +
Zookeeper, an observability stack, etc.), delegate to the Compose file:

```ini
[kafka]
compose=./docker/kafka-compose.yml
expose.kafka=9092
expose.zookeeper=2181
```

- `eph` runs `docker compose -f <file> -p eph-<short_id>-kafka up -d`, so the
  whole project is namespaced per workspace.
- `expose.<name>=<container_port>` declares a port that would be referenced as
  `${kafka.port.kafka}` (see the limitations below before relying on it).
- `ready-timeout` defaults to **60s** for compose services, and `post-start`
  hooks run on **every** `eph up` (not just the first).
- On teardown, **both** `eph down` and `eph down --rm` run `docker compose ...
  down`, which removes the compose containers either way.

> Requires the `docker compose` CLI plugin.

### Limitations of `compose=` services

Compose support is intentionally thin - `eph` shells out to `docker compose` and
lets it own the lifecycle. Two consequences to know:

- **They are not tracked by `eph status` or `eph env`.** After `eph up`, compose
  services do not appear in `eph status`, and their `expose` ports do **not**
  resolve through `eph env` (a `${kafka.port.kafka}` reference is left as the
  literal placeholder). If you need those ports as environment variables, read
  them from `docker compose port` yourself, or use a Docker `image=`/`run=`
  service, which are fully tracked.
- **`eph clean` does not remove Compose-internal volumes.** It removes only the
  named volumes you declare with `volume=` in the `.eph` file. Volumes defined
  inside the Compose file are managed by `docker compose`.

For a service whose ports you want to interpolate, prefer `image=` (or
`dockerfile=`). Reach for `compose=` when you genuinely need a pre-existing
multi-container Compose project and can live with managing its ports yourself.

## `run=` - shell command (non-Docker) services

For services that are not containers - a locally installed binary, a language
process, LocalStack, etc.:

```ini
[localstack]
run=localstack start
port=4566
env.SERVICES=s3,sqs,dynamodb
healthcheck=curl -sf http://localhost:4566/_localstack/health
```

- The command runs via `sh -c` in the workspace root, with your `env.*` added to
  the inherited environment. Its PID is tracked in state.
- **Ports are not remapped.** Unlike container services, a `run=` service binds
  whatever port its process binds; `eph` reports the **declared** `port=`
  value as-is for interpolation. Pick a port your process will actually use.
- The `healthcheck` (if any) runs on the host through `sh -c`, so full shell
  syntax works.
- `eph down` sends `SIGTERM`, waits, then `SIGKILL`. Starting an already-running
  `run` service again is a no-op (its PID is checked first).

> `run=` services need `sh` and `kill`, so on Windows they require WSL.

## Multi-port services

A service can expose several named ports. Reference them by name:

```ini
[minio]
image=minio/minio
port.api=9000
port.console=9001

S3_ENDPOINT=http://localhost:${minio.port.api}
S3_CONSOLE=http://localhost:${minio.port.console}
```

For a single-port service use `${service.port}`; for multi-port services always
use the named form.

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

DATABASE_URL=mysql://dev:dev@localhost:${mysql.port}/myapp
```

### Redis

```ini
[redis]
image=redis:7-alpine
port=6379
healthcheck=redis-cli ping

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

MONGO_URL=mongodb://dev:dev@localhost:${mongo.port}
```

> The health check runs without a shell, so keep it to a single command with no
> quotes-with-spaces. `mongosh --eval db.adminCommand(ping)` works because it is
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

S3_ENDPOINT=http://localhost:${minio.port.api}
S3_ACCESS_KEY=dev
S3_SECRET_KEY=devdevdev
```

### MailHog (catch-all SMTP + web UI)

```ini
[mailhog]
image=mailhog/mailhog
port.smtp=1025
port.web=8025

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

AMQP_URL=amqp://dev:dev@localhost:${rabbitmq.port.amqp}
```

## Next

See [Shell Integration](shell-integration.md) for getting these connection
details into your app and shell, or [Recipes](recipes.md) for end-to-end setups.
