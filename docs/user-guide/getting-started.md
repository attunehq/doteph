# Getting Started

This page takes you from nothing to a running Postgres in about five minutes.

## Prerequisites

- **Docker**, installed and running. `eph` talks to your local Docker daemon to
  start containers. Check it with `docker ps`.
- **A POSIX shell** (`sh`) and `kill` for non-container features (`run=`
  services, `post-start`/`pre-stop` hooks, and shell health checks).
  - Linux and macOS have these natively.
  - On **Windows**, run `eph` inside **WSL**. Plain Docker-image services are
    the cross-platform path; the shell-based features above require WSL. See
    [Troubleshooting](troubleshooting.md#windows-and-wsl).

## Install

From a checkout of the source:

```sh
cargo install --path .
# or
make install
```

This puts an `eph` binary on your `PATH`. Confirm it:

```sh
eph --version
```

## Write your first `.eph` file

In the root of a project, create a file named `.eph`:

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
healthcheck=pg_isready -U dev

DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
```

What each part does:

- `[postgres]` declares a service named `postgres`.
- `image=` says to run the official `postgres:16-alpine` Docker image.
- `port=5432` exposes the container's port 5432. `eph` maps it to a **random
  free port on your machine** so it never collides with anything else.
- `env.POSTGRES_*` are environment variables passed **into the container** (here
  they configure the Postgres superuser and database).
- `healthcheck=` is a command `eph` runs until it succeeds, so `eph up` only
  returns once Postgres is actually ready to accept connections.
- `DATABASE_URL=...` is a top-level environment variable for **your shell**.
  `${postgres.port}` is replaced with the real assigned host port when you run
  `eph env`.

> Comments must be on their own line, starting with `#`. A `#` after a value is
> part of the value, not a comment. See [The `.eph` File](eph-file.md#comments).

## Validate it

Before starting anything, check the file parses:

```sh
eph check
```

This reports the environment variables and services it found, or a parse error
with a line number.

## Start your services

```sh
eph up
```

`eph` pulls the image if needed, starts the container, waits for the health
check to pass, and prints the assigned port:

```
Services started:
  postgres -> localhost:54321

Run `eval "$(eph env)"` to set environment variables
```

## See what is running

```sh
eph status
```

```
Workspace: /home/you/projects/myapp
ID: a1b2c3d4

Running services:
  postgres -> localhost:54321
```

## Load the connection details into your shell

`eph env` prints shell-ready variable assignments with the real ports filled in:

```sh
eph env
# export DATABASE_URL="postgres://dev:dev@localhost:54321/myapp"
```

Load them into your current shell:

```sh
eval "$(eph env)"              # bash / zsh / sh
```

Now `$DATABASE_URL` points at your running Postgres, and your app can connect.
(fish and JSON formats are covered in [Shell Integration](shell-integration.md).)

## Stop your services

```sh
eph down
```

This stops the containers but **keeps them and their data**, so the next `eph
up` is fast. To also remove the containers:

```sh
eph down --rm
```

To wipe everything for this workspace - containers, named volumes (their data),
and saved state:

```sh
eph clean
```

## The core loop

That is the whole day-to-day workflow:

```sh
eph up                 # start services
eval "$(eph env)"      # load connection details
# ... work ...
eph down               # stop when you are done
```

## Next

You have the mechanics. Now read [Core Concepts](concepts.md) to understand
*why* it works the way it does - workspaces, isolation, ports, and the
lifecycle - which makes everything else fall into place.
