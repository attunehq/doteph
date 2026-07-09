---
title: "eph User Guide"
summary: "What eph is, and the reading order that takes you from zero to the full picture."
order: 0
---

# eph User Guide

`eph` runs ephemeral, per-workspace development services. It is `.env` for
services: you describe the Postgres, Redis, MinIO, and app processes your
project needs in a single `.eph` file, and `eph` starts them on demand,
isolated per checkout, with host ports assigned automatically.

The guide is written to be read top to bottom. Each chapter builds on the one
before it, from your first `eph up` to the full mental model, and the
reference material comes last.

## The path

1. **[Getting Started](getting-started.md)**: install `eph`, write your first
   `.eph` file, and run the core loop (`up`, `env`, `down`) in about five
   minutes.
2. **[Core Concepts](concepts.md)**: the mental model behind the tool:
   workspaces, isolation, automatic ports, persisted state, and the service
   lifecycle. Read this once and everything else follows.
3. **[The `.eph` File](eph-file.md)**: the complete file format: environment
   variables, service sections, every property, lifecycle hooks, roles, and
   interpolation.
4. **[Defining Services](services.md)**: the four ways to define a service
   (`image`, `dockerfile`, `compose`, `run`), with copy-pasteable definitions
   for Postgres, Redis, MinIO, and friends.
5. **[Running Your App](run-your-app.md)**: bring your own app into the
   workspace: `port=auto`, the `eph dev` foreground loop, watch mode, and
   Claude Desktop preview servers.
6. **[Shell Integration](shell-integration.md)**: get the resolved connection
   details into your shell, your app, your editor, or a script (bash, zsh,
   fish, JSON, direnv).
7. **[Recipes](recipes.md)**: end-to-end setups: migrating from Docker
   Compose, seeding databases, prewarming services for coding agents, CI, and
   handling secrets.
8. **[Troubleshooting](troubleshooting.md)**: the gotchas that actually bite,
   and how to diagnose a service that will not start.

## Reference

- **[Command Reference](command-reference.md)**: every command, every flag,
  and what each one prints.
- **[For Agents and Scripts](for-agents.md)**: a terse, scannable quick
  reference for AI coding agents and automation. If you are an agent working
  in a repo that uses `eph`, you can act from that page alone.

## The whole tool in one paragraph

Each directory that contains a `.eph` file is a *workspace*. `eph up` starts
the services that workspace defines (Docker containers, Compose projects, or
plain processes), names them after a hash of the workspace path so two
checkouts never collide, and lets Docker pick free host ports so nothing
conflicts. `eph env` prints the resolved connection strings for your shell to
load. `eph down` stops the services, and `eph clean` removes them and their
data entirely.
