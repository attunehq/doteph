# `.eph` File Format

The `.eph` format defines ephemeral services per workspace. It extends `.env` syntax with INI-style sections for service definitions.

## Quick Example

```eph
# Services
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
healthcheck=pg_isready -U dev
post-start=./migrate.sh

[redis]
image=redis:7-alpine
port=6379

# Environment variables with interpolation
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
REDIS_URL=redis://localhost:${redis.port}
```

## Syntax

### Environment Variables

Standard `.env` syntax - these are exported to your shell:

```eph
APP_ENV=development
DEBUG=true
API_KEY=your-secret-key
```

### Service Definitions

Services are defined in `[bracketed]` sections:

```eph
[service-name]
property=value
```

Inside a `[service]` section, a key that is not a recognized service property
but looks like a `SCREAMING_SNAKE_CASE` environment variable name is treated as
a top-level environment variable and ends the section. `eph` prints a warning
when this happens. This is why a mistyped property name (for example
`HEALTHCHECK=` instead of `healthcheck=`) may silently become a global
environment variable rather than a service property.

## Service Types

Every service must declare exactly one source: `image`, `dockerfile`, `compose`,
or `run`. A section that declares no source is rejected when the file is parsed
(for example by `eph check`), before any service is started.

### Docker Image

The most common case - pull and run a Docker image:

```eph
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=app_dev
volume=pgdata:/var/lib/postgresql/data
healthcheck=pg_isready -U dev
post-start=./scripts/migrate.sh
```

### Multi-Port Services

Some services expose multiple ports:

```eph
[minio]
image=minio/minio
port.api=9000
port.console=9001
command=server /data --console-address ":9001"
env.MINIO_ROOT_USER=admin
env.MINIO_ROOT_PASSWORD=adminadmin
```

Reference ports with `${minio.port.api}` and `${minio.port.console}`.

### Dockerfile

Build from a local Dockerfile:

```eph
[worker]
dockerfile=./docker/worker.Dockerfile
context=.
port=8080
env.WORKER_THREADS=4
```

### Docker Compose

For complex multi-container setups, delegate to docker-compose:

```eph
[kafka]
compose=./docker/kafka-compose.yml
expose.kafka=9092
expose.zookeeper=2181
```

Services are namespaced per workspace. Use `expose.<name>` to make ports available for interpolation.

### Shell Command

Run a non-Docker service:

```eph
[localstack]
run=localstack start
port=4566
env.SERVICES=s3,sqs,dynamodb
healthcheck=curl -sf http://localhost:4566/_localstack/health
```

## Service Properties

| Property | Description | Example |
|----------|-------------|---------|
| `image` | Docker image to run | `postgres:16-alpine` |
| `dockerfile` | Path to Dockerfile | `./docker/Dockerfile` |
| `context` | Build context for dockerfile | `./services/api` |
| `compose` | Path to docker-compose file | `./docker/compose.yml` |
| `run` | Shell command (non-Docker) | `localstack start` |
| `command` | Override container CMD | `server /data` |
| `port` | Single port to expose | `5432` |
| `port.<name>` | Named port | `port.api=9000` |
| `env.<KEY>` | Container environment variable | `env.POSTGRES_USER=dev` |
| `volume` | Volume mount (repeatable) | `data:/var/lib/data` |
| `healthcheck` | Command to verify service is ready | `pg_isready -U dev` |
| `ready-timeout` | Seconds to wait for healthcheck | `30` |
| `post-start` | Run after service is healthy (repeatable) | `./migrate.sh` |
| `pre-stop` | Run before stopping (repeatable) | `./backup.sh` |
| `expose.<name>` | Expose port from compose service | `expose.grafana=3000` |

## Interpolation

Reference service properties in environment variables:

```eph
[postgres]
image=postgres:16
port=5432
env.POSTGRES_USER=app

DATABASE_URL=postgres://app:pass@localhost:${postgres.port}/mydb
```

| Syntax | Description |
|--------|-------------|
| `${service.port}` | Auto-assigned host port |
| `${service.port.name}` | Named port |
| `${service.host}` | Hostname (always `localhost`) |

Service ports are published on `127.0.0.1` only, so `${service.host}` resolves
to `localhost`. Ports are reachable from your machine but are not exposed to the
local network.

## Lifecycle Hooks

### Health Checks

The `healthcheck` property specifies a command to verify the service is ready:

```eph
[postgres]
image=postgres:16
healthcheck=pg_isready -U dev
ready-timeout=30
```

The command runs inside the container. `eph up` waits for it to succeed before continuing.

### Post-Start Hooks

Run commands after a service becomes healthy:

```eph
[postgres]
image=postgres:16
healthcheck=pg_isready -U dev
post-start=cargo sqlx migrate run
post-start=cargo sqlx fixtures load
```

Hooks run sequentially in your workspace directory (not inside the container).

## Workspace Isolation

Each workspace gets isolated services based on a hash of its absolute path:

```
~/projects/myapp/       ->  eph-a1b2c3d4-postgres
~/projects/myapp-v2/    ->  eph-e5f6g7h8-postgres
```

This means:
- **Container names** are unique per workspace
- **Ports** are auto-assigned (no conflicts)
- **Volumes** are prefixed per workspace

## Complete Example

```eph
# Services
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

# Environment variables
DATABASE_URL=postgres://app:dev@localhost:${postgres.port}/myapp_dev
REDIS_URL=redis://localhost:${redis.port}
S3_ENDPOINT=http://localhost:${minio.port.api}
SMTP_HOST=localhost
SMTP_PORT=${mailhog.port.smtp}
APP_ENV=development
```
