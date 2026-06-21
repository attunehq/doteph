# eph User Guide

`eph` runs ephemeral, per-workspace development services. It is `.env` for
services: you describe the Postgres, Redis, MinIO, etc. that your project needs
in a single `.eph` file, and `eph` starts them on demand, isolated per
workspace, with ports assigned automatically.

This guide is written to be read top to bottom. Each page builds on the last:
you learn the fundamentals first, then progressively more advanced topics.

## Read in order

1. [Getting Started](getting-started.md) - install `eph`, write your first
   `.eph` file, and run the core loop (`up` -> `env` -> `down`) in about five
   minutes.
2. [Core Concepts](concepts.md) - the mental model: workspaces, isolation,
   automatic ports, persisted state, and the service lifecycle. Read this once
   and the rest of the tool makes sense.
3. [The `.eph` File](eph-file.md) - the complete file format: environment
   variables, service sections, every service property, and interpolation.
4. [Defining Services](services.md) - the four ways to define a service
   (`image`, `dockerfile`, `compose`, `run`) with copy-pasteable examples for
   common services.
5. [Shell Integration](shell-integration.md) - load service connection details
   into your shell, your app, your editor, or an agent (bash, zsh, fish, JSON).
6. [Recipes](recipes.md) - real-world setups: migrating from Docker Compose,
   using `eph` in CI, multiple checkouts side by side, seeding databases,
   handling secrets.
7. [Troubleshooting](troubleshooting.md) - the gotchas that bite people, and
   how to diagnose a service that will not start.

## Reference

- [Command Reference](command-reference.md) - every command, every flag, and
  the exact output each one prints.
- [For Agents and Scripts](for-agents.md) - a terse, scannable quick reference
  for AI coding agents and automation. If you are an agent working in a repo
  that uses `eph`, start here.

## In one paragraph

Each directory that contains a `.eph` file is a *workspace*. `eph up` starts the
services that workspace defines as Docker containers (or Compose projects, or
plain processes), names them after a hash of the workspace path so two checkouts
never collide, and lets Docker pick free host ports so nothing conflicts.
`eph env` prints the resolved connection strings for your shell to load. `eph
down` stops the services; `eph clean` removes them and their data entirely.
