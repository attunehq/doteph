# Troubleshooting

The gotchas that actually bite, and how to diagnose a service that will not
start. When something is wrong, two commands tell you most of what you need:

```sh
eph check        # does the .eph file parse?
eph -v up        # start with verbose (debug) logging on stderr
```

## "failed to connect to docker (is docker running?)"

`eph` talks to your local Docker daemon. Start Docker (Docker Desktop, Colima,
`systemctl start docker`, etc.) and confirm with `docker ps`. On macOS/Windows
make sure the Docker Desktop VM is fully started, not just launching.

## "no .eph file found in ... or any parent directory"

You are not inside a workspace. `eph` searches the current directory and walks
**up** to find a `.eph` file. Either `cd` into your project or create a `.eph`
file. Confirm what `eph` resolves with `eph info`.

## A service fails to become healthy

```
service postgres failed to become healthy within 30s
```

Causes, in rough order of likelihood:

1. **The health check uses shell features.** For `image`/`dockerfile` services
   the health check runs **inside the container without a shell** - it is split
   on whitespace and exec'd directly. Pipes (`|`), `&&`, redirects, `$VAR`, and
   quoted arguments containing spaces do **not** work. Use one plain command,
   e.g. `pg_isready -U dev`, `redis-cli ping`. (For `run`/`compose` services the
   check runs through the platform shell (`sh -c` on Unix, `cmd /C` on Windows),
   so shell syntax is fine there.)
2. **The check binary is not in the image.** `mysqladmin`, `mongosh`,
   `pg_isready`, `curl` must exist inside the container. Slim images may omit
   them.
3. **The service genuinely needs longer.** Raise `ready-timeout=` (seconds).
4. **The service crashed on startup.** Inspect its logs with
   `eph logs <service>` (works for every service type, and shows a `run=`
   service's output even after it exited). For Docker-backed services you can
   also go straight to the daemon: `docker logs eph-<short_id>-<service>` (get
   the name from `eph info` + service name, or `docker ps -a`).

Run `eph -v up` to watch each health-check attempt and its exit code.

## A property was ignored

If a service property seems to have no effect, you may have a typo. Two distinct
behaviors:

- A **lowercase** unknown property is a hard error: `unknown service property
  'prot' at line N`. `eph check` will catch it.
- An **UPPERCASE** unknown key is silently reclassified as a top-level
  environment variable and **ends the section** - with a warning on stderr. So
  `HEALTHCHECK=pg_isready` (wrong case) becomes a global variable named
  `HEALTHCHECK`, not a health check, and any lines after it are no longer part of
  the service. Property names are lowercase: `image`, `port`, `env.X`,
  `healthcheck`, `post-start`, etc.

Run `eph -v check` to see the reclassification warnings.

> One reclassification warning is **normal**: if you put your top-level
> environment variables after your service sections (the layout used throughout
> this guide), the first variable ends the last section and emits a single
> warning. The file still parses correctly. Put top-level variables before the
> sections if you want to silence it.

## An inline comment broke a value

Comments must be on their own line. There are **no trailing comments** - a `#`
after a value is part of the value:

```ini
port=5432   # this whole thing is the value, and fails to parse
```

Symptoms: `invalid port number at line N`, or an image/URL that mysteriously has
` # ...` appended. Move the comment to its own line above.

## `post-start` ran again and broke (or duplicated data)

`post-start` hooks run on **every** `eph up`, including when a stopped container
is restarted -- not just on first creation. A hook that is not idempotent (a
plain `INSERT` seed, a one-shot setup script) will repeat its effect and may
fail or duplicate rows on the second `eph up`.

Fixes:

- Make the hook idempotent: a migration that no-ops when already applied, an
  `INSERT ... ON CONFLICT DO NOTHING` seed, `CREATE TABLE IF NOT EXISTS`.
- Move one-off or destructive work out of `post-start` and run it explicitly
  with [`eph run`](command-reference.md#eph-run-cmd) when you actually want it.

## `eph down` or `eph clean` fails on a `pre-stop` hook

A failing `pre-stop` hook **aborts** the teardown and leaves the service running,
so the hook (a backup, a drain) can be fixed and retried rather than silently
skipped. If a broken `pre-stop` is wedging teardown:

- Fix the hook and re-run `eph down` / `eph clean`.
- Or skip the hooks for this teardown: `eph down --skip-hooks` /
  `eph clean --skip-hooks`.

## A port reference did not resolve

If `eph env` leaves a literal `${service.port}` in its output:

- The **service is not running.** Run `eph up` first; interpolation only resolves
  against running services. Check with `eph status`.
- The **name is wrong.** `${db.port}` only resolves if the section is `[db]`.
- It is a **multi-port service.** `${minio.port}` is not well-defined when a
  service declares several ports; use the named form `${minio.port.api}`. The
  same applies to a `compose` service: reference each `expose.<name>=` port as
  `${service.port.<name>}`, not `${service.port}`.

## A `run=` service is on the wrong port

`run=` (non-container) services are **not** port-remapped. The process binds
whatever port it binds, and `eph` reports the **declared** `port=` value
verbatim for interpolation. Make the declared port match the port your process
actually listens on. (Container services, by contrast, always get a random host
port.)

## Stale state or "ghost" services

`eph status` reconciles saved state against the live Docker daemon and tracked
PIDs, so a container you removed manually will drop out of `status`. If state
ever looks wrong, `eph clean` resets the workspace completely (removing
containers, named volumes, and the state file), after which `eph up` rebuilds
from scratch.

## Windows

`eph` runs natively on Linux, macOS, and Windows. The Docker-backed services
(`image=`, `dockerfile=`, `compose=`) behave identically everywhere.

The shell-based features (`run=` services, `post-start`/`pre-stop` hooks, and
`run`/`compose` health checks) run through the platform shell: `sh -c` on Unix
and `cmd /C` on Windows. Process liveness and teardown are native (no POSIX
`kill`). Teardown stops the whole process tree a `run=` command spawned, so a
compound command's children are not orphaned: on Unix the command runs in its own
process group and the stop signals the group (`SIGTERM` then `SIGKILL`); on
Windows, which has no `SIGTERM`, a stop force-terminates the command and its
descendants. The *features* therefore work natively on Windows with no WSL.

What does not cross over automatically is the command string itself: a `run=`,
hook, or health-check command written for `sh` (pipes, `$VAR`, `&&`, and POSIX
tools like `curl`/`pg_isready`) may need a `cmd`-compatible rewrite on Windows
(`%VAR%`, different builtins). Two ways to handle it:

- Write the command for `cmd`, or call a cross-platform binary directly (many,
  like `pg_isready` or `redis-cli`, take the same arguments on both platforms).
- Keep your POSIX command strings and run `eph` inside **WSL**, where it is a
  Linux process and `sh` is the shell.

When you run under WSL, `eph` is a Linux process, so its state directory is the
**Linux** path (`~/.local/share/eph/<short_id>/`), not the Windows
`%LOCALAPPDATA%` path. The `%LOCALAPPDATA%` location applies only to a native
Windows build.

## Getting more detail

`eph -v <command>` enables debug logging (to stderr). For a service's own
output, use `eph logs <service>` (add `-f` to follow). For Docker-level issues,
drop to the `docker` CLI using the names from `eph info`:

```sh
docker ps -a | grep eph-<short_id>          # containers for this workspace
docker logs eph-<short_id>-<service>        # a service's logs (or: eph logs <service>)
docker volume ls | grep eph-<short_id>      # named volumes for this workspace
```
