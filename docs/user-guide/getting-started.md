---
title: "Getting Started"
summary: "Install eph, write your first .eph file, and run the core loop in five minutes."
order: 1
---

# Getting Started

This page takes you from nothing to a running, health-checked Postgres with its
connection string loaded in your shell. Budget about five minutes.

## Prerequisites

- **Docker**, installed and running. `eph` talks to your local Docker daemon to
  start containers. Confirm with `docker ps`.
- **A shell.** The non-container features (`run=` services, lifecycle hooks,
  and shell health checks) run through the platform shell: `sh -c` on Linux and
  macOS, `cmd /C` on Windows. Everything works natively on all three platforms;
  the one catch is that a command string written for `sh` (pipes, `$VAR`,
  POSIX tools) may need a `cmd`-compatible form on Windows, or you can run
  `eph` inside WSL. See [Troubleshooting](troubleshooting.md#windows).

## Install

Install the latest release binary. The script verifies a SHA-256 checksum
before installing:

```sh
# Linux / macOS
curl -sSfL https://raw.githubusercontent.com/attunehq/doteph/main/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/attunehq/doteph/main/scripts/install.ps1 | iex
```

Or build from a source checkout:

```sh
cargo install --path .
# or
make install
```

Confirm the binary is on your `PATH`:

```sh
eph --version
```

Keep it current later with the built-in updater: `eph update` installs the
latest release (checksum-verified, swapped in place), and `eph update --check`
just reports whether one exists. Details in the
[Command Reference](command-reference.md#eph-update---check---force).

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

Reading it line by line:

- `[postgres]` declares a service named `postgres`.
- `image=` runs the official `postgres:16-alpine` Docker image.
- `port=5432` publishes the container's port 5432 on a **random free port on
  your machine**, so it never collides with anything else.
- `env.POSTGRES_*` are environment variables passed **into the container**;
  here they configure the Postgres superuser and database.
- `healthcheck=` is a command `eph` runs repeatedly until it succeeds, so
  `eph up` only returns once Postgres actually accepts connections.
- `DATABASE_URL=...` is a top-level environment variable for **your shell**.
  `${postgres.port}` is replaced with the real assigned host port when you run
  `eph env`.

> Comments must be on their own line, starting with `#`. A `#` after a value is
> part of the value, not a comment. See [The `.eph` File](eph-file.md#comments).

## Validate it

Before starting anything, check that the file parses:

```sh
eph check
```

This reports the environment variables and services it found, or a parse error
with a line number. It never touches Docker, so it is always safe to run.

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

`eph env` prints shell-ready variable assignments with the real ports filled
in:

```sh
$ eph env
export DATABASE_URL="postgres://dev:dev@localhost:54321/myapp"
```

Load them into your current shell:

```sh
eval "$(eph env)"              # bash / zsh / sh
```

Now `$DATABASE_URL` points at your running Postgres, and your app can connect.
fish and JSON formats are covered in
[Shell Integration](shell-integration.md).

## Stop your services

```sh
eph down
```

This stops the container but **keeps it and its data**, so the next `eph up`
is fast. To also remove the container:

```sh
eph down --rm
```

To wipe everything for this workspace (containers, named volumes and their
data, and saved state):

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
*why* it works this way (workspaces, isolation, ports, and the lifecycle),
which makes everything else in the guide fall into place.
