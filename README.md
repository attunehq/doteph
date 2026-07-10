# eph

Ephemeral services per workspace. Like `.env` files, but for services.

When you work on multiple projects, or multiple checkouts of the same project,
you need local services like Postgres, Redis, or MinIO without ports colliding,
data getting mixed up, or containers running all the time. `eph` gives each
workspace its own isolated services, started on demand, with host ports
assigned automatically:

```
~/projects/app/      ->  eph-a1b2c3d4-postgres (localhost:54321)
~/projects/app-v2/   ->  eph-e5f6g7h8-postgres (localhost:54322)
```

You describe the services in a `.eph` file, run `eph up`, and load the
resolved connection strings into your shell with `eval "$(eph env)"`.

## Install

Install the latest release binary (the script verifies a SHA-256 checksum
before installing):

```sh
# Linux / macOS
curl -sSfL https://raw.githubusercontent.com/attunehq/doteph/main/scripts/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/attunehq/doteph/main/scripts/install.ps1 | iex
```

Pass `-v X.Y.Z` (shell) or set `$env:Version` (PowerShell) to pin a version,
or download a `tar.gz` directly from the
[releases page](https://github.com/attunehq/doteph/releases). Prebuilt
binaries cover macOS (x86_64, arm64), Linux glibc and musl (x86_64, arm64),
and Windows (x86_64).

Once installed, keep it current with the built-in updater, which downloads the
latest release, verifies its SHA-256 checksum, and swaps the binary in place:

```sh
eph update           # install the latest release
eph update --check   # just report whether one is available
```

Or build from a source checkout:

```sh
cargo install --path .
# or
make install
```

`eph` runs natively on Linux, macOS, and Windows. The Docker-backed services
(`image=`, `dockerfile=`, `compose=`) behave identically everywhere. The
shell-based features (`run=` services, lifecycle hooks, and shell health
checks) run through the platform shell (`sh -c` on Unix, `cmd /C` on
Windows), so a command string written for `sh` may need a `cmd`-compatible
form; to keep writing POSIX command strings on Windows, run eph inside
[WSL](docs/user-guide/troubleshooting.md#windows).

## Quick taste

Describe your services in a `.eph` file:

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
healthcheck=pg_isready -U dev

[env]
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
```

Then run the loop:

```sh
eph up                 # start services (waits until healthy)
eval "$(eph env)"      # load connection strings into your shell
eph down               # stop when you are done
```

Your own app fits in the same file as a `run=` service with `port=auto`, so
eph allocates its port, injects the resolved environment, and (with
[`eph dev`](docs/user-guide/run-your-app.md)) runs the whole stack as one
foreground command. Tag services with roles (`role=dep`, `role=app`) to bring
up one tier at a time, for example
[prewarming databases](docs/user-guide/recipes.md#prewarm-dependency-services-on-claude-code-session-start)
in a Claude Code SessionStart hook without starting your app.

## Documentation

The full guide lives at
**[attunehq.github.io/doteph](https://attunehq.github.io/doteph)**, and the
same pages are readable in-repo:

- **[User Guide](docs/user-guide/README.md)**: a guided path from install to
  full understanding: getting started, concepts, the `.eph` format, services,
  running your app, shell integration, recipes, troubleshooting, and a
  complete command reference. New here? Start with
  [Getting Started](docs/user-guide/getting-started.md).
- **[For Agents and Scripts](docs/user-guide/for-agents.md)**: a terse quick
  reference for AI coding agents and automation. Run `eph skills install` to
  drop that guidance into a repo as a skill your agent discovers
  automatically.
- **[Developer Guide](docs/developer-guide/README.md)**: architecture,
  building and testing, and a tour of the internals, for working on `eph`
  itself.
- **[Contributing](CONTRIBUTING.md)**: working style, pull requests,
  releases.

## License

`eph` is licensed under the [MIT License](LICENSE), Copyright (c) 2026
Attune, Inc.

The repository also vendors third-party Rust coding-guidance skills under
[`.agents/skills/`](.agents/skills/) (notably the MIT-licensed
[`rust-skills`](.agents/skills/rust-skills/) pack). See [NOTICE](NOTICE) for
attribution and the licenses of vendored and externally sourced material.
