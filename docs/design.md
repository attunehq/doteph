# Design

This document explains the key design decisions behind `eph`.

## Core Concepts

### Workspace Isolation

Each directory containing a `.eph` file is a workspace. Workspaces are identified by a SHA256 hash of their absolute path:

```
/Users/alice/projects/myapp  ->  eph-a1b2c3d4
/Users/alice/projects/myapp2 ->  eph-e5f6g7h8
```

This ensures that:
- Two checkouts of the same repo don't conflict
- Multiple developers on the same machine don't conflict
- You can run the same project in multiple terminals simultaneously

### Auto Port Assignment

Container ports are mapped to randomly-assigned host ports by Docker. This eliminates port conflicts entirely - you never need to manually pick ports or worry about collisions.

Host ports are published on `127.0.0.1` only. Services are reachable from your machine but are not exposed to the local network.

The assigned ports are tracked in state and exposed via interpolation:

```eph
[postgres]
port=5432  # Container port (fixed)

# Host port is auto-assigned, e.g., 54321
DATABASE_URL=postgres://localhost:${postgres.port}/db
```

### Service State

Running service information is persisted to the platform local-data directory under `eph/{workspace_id}/state.json`:

- Linux: `~/.local/share/eph/{workspace_id}/state.json`
- macOS: `~/Library/Application Support/eph/{workspace_id}/state.json`
- Windows: `%LOCALAPPDATA%\eph\{workspace_id}\state.json`

This allows:
- `eph status` to show running services without querying Docker
- `eph env` to resolve interpolations using saved port mappings
- Services to survive terminal restarts

## File Format

The `.eph` format was designed with these goals:

1. **Familiar** - Looks like `.env` + INI files developers already know
2. **Minimal** - Simple cases require minimal syntax
3. **Flat** - No deep nesting or indentation requirements

### Why Not YAML/TOML/JSON?

| Format | Issue |
|--------|-------|
| YAML | Indentation errors, type coercion surprises |
| TOML | Verbose for this use case, requires quotes |
| JSON | No comments, not human-friendly |
| HCL | Learning curve, overkill |

The `.eph` format is essentially `.env` with sections. A valid `.env` file is a valid `.eph` file.

## Service Types

### Docker Images (default)

Most services are Docker images. `eph` handles:
- Pulling images if needed
- Creating containers with workspace-prefixed names
- Port mapping with auto-assignment
- Volume creation with workspace prefixes
- Health check polling

### Dockerfiles

For custom images, `dockerfile=` builds locally:

```eph
[worker]
dockerfile=./docker/Dockerfile
context=.
```

The image is tagged `eph-{workspace_id}-{service}` and cached.

### Docker Compose

For complex multi-container setups, delegate to compose:

```eph
[kafka]
compose=./docker/kafka-compose.yml
expose.kafka=9092
```

The compose project is namespaced per workspace.

### Shell Commands

For non-Docker services (e.g., `localstack`, native processes):

```eph
[localstack]
run=localstack start
port=4566
```

The process runs in the background; its PID is tracked in state.

## Health Checks

Health checks run inside containers using `docker exec`:

```eph
[postgres]
healthcheck=pg_isready -U dev
ready-timeout=30
```

`eph up` waits for health checks to pass before:
1. Running `post-start` hooks
2. Returning success

For shell command services, health checks run as local shell commands.

## Lifecycle Hooks

### post-start

Runs in your workspace directory after the service is healthy:

```eph
post-start=cargo sqlx migrate run
post-start=./seed-data.sh
```

Common uses:
- Database migrations
- Fixture loading
- Cache warming

### pre-stop

Runs before stopping a service:

```eph
pre-stop=./backup-db.sh
```

## Interpolation

Environment variables can reference service properties:

```eph
DATABASE_URL=postgres://localhost:${postgres.port}/db
```

Supported references:
- `${service.port}` - Primary port
- `${service.port.name}` - Named port
- `${service.host}` - Always `localhost` (ports are published on `127.0.0.1` only and are not exposed to the local network)

Interpolation is resolved at runtime by `eph env`, using the actual assigned ports from the running services.

## CLI Design

Commands follow a simple pattern:

```
eph up [services...]            # Start
eph down [--rm] [services...]   # Stop; --rm (-r) also removes the stopped containers
eph clean                       # Full reset: remove all eph containers and per-workspace
                                # named volumes, and clear persisted state (deletes volume data)
eph status                      # Show state
eph env                         # Export environment
```

### Reaping containers

There are two levels of teardown:

- `eph down` stops services but leaves their containers (and volumes) in place, so a later `eph up` can reuse them. With `--rm` (`-r`) it also removes the stopped containers.
- `eph clean` is a full reset for the workspace: it removes all eph containers and the per-workspace named volumes, and clears persisted state. Because it deletes the named volumes, any data they hold is lost.

The `env` command outputs shell-compatible export statements:

```bash
eval "$(eph env)"        # bash/zsh
eval (eph env -f fish)   # fish
```

This integrates with existing shell workflows without requiring hooks or special shell integration.
