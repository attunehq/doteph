# eph

Ephemeral services per workspace. Like `.env` files, but for services.

## The Problem

When working on multiple projects (or multiple checkouts of the same project), you often need local services like Postgres, Redis, or MinIO. The typical solutions have drawbacks:

- **Shared services**: Projects compete for the same database, ports conflict, data gets mixed up
- **Docker Compose per project**: Services run all the time, eating resources even when you're not working on that project
- **Manual management**: Forgetting to start services, wrong ports, stale containers

## The Solution

`eph` gives each workspace its own isolated services with automatic port assignment:

```
~/projects/app-1/     ->  eph-a1b2c3d4-postgres (port 54321)
~/projects/app-2/     ->  eph-e5f6g7h8-postgres (port 54322)
~/projects/app-1-v2/  ->  eph-i9j0k1l2-postgres (port 54323)
```

Services start when you need them and stay out of your way when you don't.

## Quick Start

### Install

```bash
# From source
cargo install --path .

# Or using make
make install
```

### Create a `.eph` file

```bash
cat > .eph << 'EOF'
# Services
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
healthcheck=pg_isready -U dev

[redis]
image=redis:7-alpine
port=6379

# Environment variables (with interpolation)
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
REDIS_URL=redis://localhost:${redis.port}
EOF
```

### Use it

```bash
# Start services
eph up

# Check what's running
eph status

# Get environment variables
eph env              # bash/zsh export format
eph env -f fish      # fish format
eph env -f json      # JSON format

# Load into your shell
eval "$(eph env)"                            # bash/zsh
eval (eph env -f fish | string collect)      # fish (preserves newlines)
# or: source (eph env -f fish | psub)

# Stop services
eph down
```

## `.eph` File Format

The format extends `.env` syntax with INI-style sections for services:

```ini
# Plain environment variables (like .env)
APP_NAME=myapp
DEBUG=true

# Service definitions
[postgres]
image=postgres:16                    # Docker image
port=5432                            # Container port (host port auto-assigned)
env.POSTGRES_USER=dev                # Environment variables for the container
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=app
volume=pgdata:/var/lib/postgresql    # Named volumes (prefixed per-workspace)
healthcheck=pg_isready -U dev        # Health check command
ready-timeout=30                     # Seconds to wait for healthy
post-start=cargo sqlx migrate run    # Run after service is healthy

# Multiple named ports
[minio]
image=minio/minio
port.api=9000
port.console=9001
command=server /data --console-address ":9001"

# Environment variables with interpolation
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/app
S3_ENDPOINT=http://localhost:${minio.port.api}
```

### Service Properties

| Property | Description |
|----------|-------------|
| `image=` | Docker image to pull and run |
| `dockerfile=` | Build from Dockerfile instead |
| `context=` | Build context for `dockerfile=` |
| `compose=` | Use docker-compose file |
| `run=` | Shell command (non-Docker) |
| `command=` | Override container command |
| `port=` | Single port to expose |
| `port.<name>=` | Named port to expose |
| `env.<KEY>=` | Environment variable for the container |
| `volume=` | Volume mount (`name:path` or `./host:path`) |
| `healthcheck=` | Command to check if service is ready |
| `ready-timeout=` | Seconds to wait for healthcheck (default: 30) |
| `post-start=` | Command to run after service is healthy |
| `pre-stop=` | Command to run before stopping |
| `expose.<name>=` | Expose a port from a `compose=` service for interpolation |

### Interpolation

Environment variables can reference service properties:

- `${service.port}` - The assigned host port (for single-port services)
- `${service.port.name}` - A named port
- `${service.host}` - Always `localhost`

Service ports are published on `127.0.0.1` only. They are reachable from your
machine but are not exposed to the local network.

### Secrets

A `.eph` file may contain credentials (for example `env.POSTGRES_PASSWORD`).
`eph env` prints these values to stdout so they can be loaded into your shell,
so treat the file accordingly: use throwaway, dev-only credentials, or add the
`.eph` file to `.gitignore` if it holds anything sensitive.

## Commands

```
eph up [service...]         Start services (all or specific ones)
eph down [--rm] [service...] Stop services; --rm (-r) also removes the stopped containers
eph clean                   Full reset: remove all eph containers and per-workspace
                            named volumes, and clear persisted state (deletes volume data)
eph status                  Show running services and their ports
eph env [-f format]         Print environment variables
eph check                   Validate .eph file
eph info                    Show workspace info (ID, paths)
```

## How It Works

1. **Workspace ID**: Each directory with a `.eph` file gets a unique ID (SHA256 of absolute path)
2. **Container names**: Services are named `eph-{short_id}-{service}` to avoid conflicts
3. **Port assignment**: Docker assigns random available ports; eph tracks them. Ports are published on `127.0.0.1` only, so they are reachable from your machine but not exposed to the local network
4. **State**: Running service info is persisted to the platform local-data directory under `eph/{short_id}/`:
   - Linux: `~/.local/share/eph/{short_id}/`
   - macOS: `~/Library/Application Support/eph/{short_id}/`
   - Windows: `%LOCALAPPDATA%\eph\{short_id}\`

## Platform Support

`eph` runs natively on Linux and macOS. On Windows it requires WSL, because
shell command services (`run=`), `post-start`/`pre-stop` hooks, shell health
checks, and process management all shell out to `sh` and `kill`. Docker-image
services are the cross-platform path.

## Development

```bash
make dev              # Build debug
make release          # Build release
make test             # Run all tests
make test-integration # Run integration tests (requires Docker)
make check            # Run clippy
make format           # Format code
make precommit        # Run all checks before committing
```
