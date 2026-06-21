# eph

Ephemeral services per workspace. Like `.env` files, but for services.

When you work on multiple projects - or multiple checkouts of the same project -
you need local services like Postgres, Redis, or MinIO without ports colliding,
data getting mixed up, or containers running all the time. `eph` gives each
workspace its own isolated services, started on demand, with host ports assigned
automatically:

```
~/projects/app/      ->  eph-a1b2c3d4-postgres (localhost:54321)
~/projects/app-v2/   ->  eph-e5f6g7h8-postgres (localhost:54322)
```

You describe the services in a `.eph` file, run `eph up`, and load the resolved
connection strings into your shell with `eval "$(eph env)"`.

## Install

```sh
cargo install --path .   # from a source checkout
# or
make install
```

`eph` runs natively on Linux and macOS. On Windows, run it inside WSL.

## Quick taste

Describe your services in a `.eph` file, then:

```sh
eph up                 # start services (waits until healthy)
eval "$(eph env)"      # load connection strings into your shell
eph down               # stop when you are done
```

[Getting Started](docs/user-guide/getting-started.md) walks through writing the
`.eph` file from scratch.

## Documentation

- **[User Guide](docs/user-guide/README.md)** - install, concepts, the `.eph`
  format, defining services, shell integration, recipes, troubleshooting, and a
  full command reference. New here? Start with
  [Getting Started](docs/user-guide/getting-started.md).
- **[For Agents and Scripts](docs/user-guide/for-agents.md)** - a terse quick
  reference for AI coding agents and automation.
- **[Developer Guide](docs/developer-guide/README.md)** - architecture, building
  and testing, and a tour of the internals, for working on `eph` itself.
- **[Contributing](CONTRIBUTING.md)** - working style, pull requests, releases.

## License

`eph` is licensed under the [MIT License](LICENSE), Copyright (c) 2026
Attune, Inc.

The repository also vendors third-party Rust coding-guidance skills under
[`.agents/skills/`](.agents/skills/) (notably the MIT-licensed
[`rust-skills`](.agents/skills/rust-skills/) pack). See [NOTICE](NOTICE) for
attribution and the licenses of vendored and externally sourced material.
