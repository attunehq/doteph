---
title: "Troubleshooting"
summary: "The gotchas that bite, and how to diagnose a service that will not start."
order: 8
---

# Troubleshooting

The gotchas that actually bite, and how to diagnose a service that will not
start. When something is wrong, two commands tell you most of what you need:

```sh
eph check        # does the .eph file parse?
eph -v up        # start with verbose (debug) logging on stderr
```

## "failed to connect to docker (is docker running?)"

`eph` talks to your local Docker daemon. Start Docker (Docker Desktop, Colima,
`systemctl start docker`, whichever you use) and confirm with `docker ps`. On
macOS and Windows, make sure the Docker Desktop VM has fully started, not just
begun launching.

## "no .eph file found in ... or any parent directory"

You are not inside a workspace. `eph` searches the current directory and walks
**up** to find a `.eph` file. Either `cd` into your project or create a `.eph`
file. Confirm what `eph` resolves with `eph info`.

## A service fails to become healthy

```
service postgres failed to become healthy within 30s
```

Causes, in rough order of likelihood:

1. **The health check uses shell features.** For `image` and `dockerfile`
   services the health check runs **inside the container without a shell**: it
   is split on whitespace and exec'd directly. Pipes (`|`), `&&`, redirects,
   `$VAR`, and quoted arguments containing spaces do **not** work. Use one
   plain command, such as `pg_isready -U dev` or `redis-cli ping`. (For `run`
   and `compose` services the check runs through the platform shell, so shell
   syntax is fine there.)
2. **The check binary is not in the image.** `mysqladmin`, `mongosh`,
   `pg_isready`, and `curl` must exist inside the container. Slim images may
   omit them.
3. **The service genuinely needs longer.** Raise `ready-timeout=` (seconds).
4. **The service crashed on startup.** Inspect its logs with
   `eph logs <service>`, which works for every service type and shows a `run=`
   service's output even after it exited. For Docker-backed services you can
   also go straight to the daemon: `docker logs eph-<short_id>-<service>`
   (compose the name from `eph info` plus the service name, or find it with
   `docker ps -a`).

Run `eph -v up` to watch each health-check attempt and its exit code.

## A property was rejected

Any typo'd or misplaced service property is a hard parse error; `eph check`
catches it before any `eph up`. Two shapes show up in practice:

- **A lowercase typo** is an unknown-property error listing every known
  property name:

  ```
  unknown service property 'prot' at line 5 (known properties: image,
  dockerfile, compose, run, role, command, port, port.<name>, expose.<name>,
  env.<KEY>, volume, pre-start, post-start, pre-stop, post-stop, healthcheck,
  ready-timeout, context)
  ```

  Property names are lowercase: `image`, `port`, `env.X`, `healthcheck`,
  `post-start`, and so on.

- **An UPPERCASE key inside a section** (the classic `HEALTHCHECK=...` instead
  of `env.HEALTHCHECK=...`, or a top-level variable you meant to put after this
  service) is rejected rather than silently absorbed as a top-level variable:

  ```
  'HEALTHCHECK' at line 5 looks like an environment variable, but it is
  inside service 'postgres' (sections do not end at blank lines). To set it
  for the service, write env.HEALTHCHECK=...; to export it from `eph env`,
  move it into an [env] section or above the first section
  ```

  The error names both possible intents so you can pick the right fix: prefix
  it with `env.` if it belongs to the service, or move it into an `[env]`
  section (or above the first section) if it belongs to your shell. See
  [Where to put top-level variables](eph-file.md#where-to-put-top-level-variables).

## Something is "duplicate"

Nothing in a `.eph` file is silently merged or overwritten. A repeated
declaration is a parse error naming both occurrences:

- **A reopened section** (`[db]` appearing twice) does not merge the two
  blocks; the second `[db]` is rejected, naming the line the first one started
  on.
- **A repeated single-valued property** (a second `image=`, `healthcheck=`,
  `command=`, `ready-timeout=`, or a second source under any spelling, such as
  `image=` followed by `run=`) is rejected. Hooks and `volume=` are the
  exception: they are designed to repeat and accumulate.
- **A repeated port or `env.` key** (`port=` twice, the same `port.<name>=`
  twice, the same `env.KEY=` twice) is rejected; give each a distinct name
  instead.
- **A repeated top-level variable name** is rejected, whether both
  declarations are above the first section, both inside `[env]` sections, or
  split across the two: the top-of-file block and every `[env]` section share
  one namespace.

Give the second occurrence a different name, or delete it if it was a
leftover from editing.

## An interpolation reference did not parse

`${service.property}` placeholders in a top-level variable or `env.<KEY>=`
value are validated at parse time, before any `eph up`:

- **`unterminated '${' ...`**: a `${` with no closing `}`. Close it, or if you
  meant a literal `${`, escape it as `$${`.
- **`invalid interpolation ... expected ${service.property}`**: the text
  inside `${...}` has no `.`, so it cannot name a service and a property (for
  example `${name}` instead of `${web.port}`). Add the missing part, or escape
  it as `$${` if it was never meant to be a placeholder.
- **`unknown service '...' referenced from ...`**: the placeholder names a
  service that is not defined anywhere in the file. Check the spelling against
  the section header; a service defined later in the file resolves fine, so
  this is always a genuine typo or a missing section.

These are different from [a port reference that does not resolve](#a-port-reference-did-not-resolve):
that is a well-formed reference to a real service that is not running.

## An inline comment broke a value

Comments must be on their own line. There are **no trailing comments**; a `#`
after a value is part of the value:

```ini
port=5432   # this whole thing is the value, and fails to parse
```

Symptoms: `invalid port number at line N`, or an image name or URL that
mysteriously has ` # ...` appended. Move the comment to its own line above.

## `pre-start` or `post-start` ran again and broke (or duplicated data)

`pre-start` and `post-start` hooks run on **every** `eph up`, including when a
stopped container is restarted, not just on first creation. A hook that is not
idempotent (a plain `INSERT` seed, a one-shot setup script) repeats its effect
and may fail or duplicate rows on the second `eph up`.

Fixes:

- Make the hook idempotent: a migration that no-ops when already applied, an
  `INSERT ... ON CONFLICT DO NOTHING` seed, `CREATE TABLE IF NOT EXISTS`,
  codegen that overwrites its output in place.
- Move one-off or destructive work out of the hook and run it explicitly with
  [`eph run`](command-reference.md#eph-run-cmd) when you actually want it.

## `eph down` or `eph clean` fails on a `pre-stop` or `post-stop` hook

A failing `pre-stop` hook **aborts** the teardown and leaves the service
running, so the hook (a backup, a drain) can be fixed and retried rather than
silently skipped. A failing `post-stop` hook also aborts the rest of the
teardown, but its own service is already stopped, so re-running `eph down`
will not run that `post-stop` again. If a broken hook is wedging teardown:

- Fix the hook, then re-run `eph down` or `eph clean`. A `pre-stop` retries on
  the re-run; a `post-stop` whose service already stopped must be run by hand,
  for example with [`eph run`](command-reference.md#eph-run-cmd).
- Or skip the hooks for this teardown: `eph down --skip-hooks` or
  `eph clean --skip-hooks`.

## A port reference did not resolve

`eph check` rejects unknown services, unknown properties, missing named ports,
and ambiguous bare port references. A valid reference can still be unavailable
at runtime when its service is stopped. In that case, `eph env` unsets the
affected shell variable or omits the JSON key, warns on stderr, and exits
non-zero. Other execution paths fail before launching a child.

- **The service is not running.** Interpolation only resolves against running
  services. Run `eph up` first, and check with `eph status`.
- **The name is wrong.** `${db.port}` only resolves if the section is `[db]`;
  `eph check` reports this before runtime.
- **It is a multi-port service.** `${minio.port}` is rejected when a service
  declares several ports; use `${minio.port.api}`. Compose mappings use
  `expose.<alias>=<compose-service>:<port>` and resolve as
  `${service.port.<alias>}`.

## A `run=` service is on the wrong port

With a numeric `port=`, a `run=` service is **not** port-remapped: the process
binds whatever it binds, and `eph` reports the declared value verbatim for
interpolation. Make the declared port match the port your process actually
listens on, or switch to `port=auto` and let eph allocate and inject it (see
[Running Your App](run-your-app.md#portauto)). If you use `port=auto` and the
app still lands elsewhere, your framework is ignoring its injected `PORT`;
enable its strict-port mode.

## Stale state or "ghost" services

`eph status` reconciles saved state against the live Docker daemon and tracked
PIDs, so a container you removed manually simply drops out of `status`. If
state ever looks wrong, `eph clean` resets the workspace completely (removing
containers, named volumes, and the state file), after which `eph up` rebuilds
from scratch.

Renaming or deleting a service's section from the `.eph` file does not orphan
its container: a bare `eph down` and `eph clean` both also tear down whatever
`state.json` remembers starting under a name no longer in the file. If you
still find a container `docker ps -a` shows but `eph status` does not, run
`eph clean`, which additionally sweeps any leftover container or volume
carrying the workspace's `eph-<short_id>-` prefix even if state does not know
about it.

## "state file ... is corrupt"

If `state.json` cannot be parsed (a hand edit, disk corruption, damage from
something outside eph), the next `eph` command logs a warning, moves the
broken file aside to `state.json.corrupt`, and continues as if the workspace
had no recorded state, rather than aborting. Containers and compose projects
are found again from Docker by name on the next `eph up` or `eph status`; a
`run=` service's PID is the one thing this cannot recover, since the PID lived
only in the corrupted file. Inspect the host process table and stop a leftover
process by hand; `eph status` and `docker ps` cannot discover it.

## "another eph command is running in this workspace; waiting for it"

`eph up`, `eph down`, and `eph clean` on one workspace exclude each other with
an OS-level lock, so two overlapping runs never race the same `state.json`.
This message just means a second command has to wait; it clears on its own
once the first command finishes. If it never clears, the first command is
still genuinely running (check `eph -v up`'s output or `docker ps`) rather
than stuck: the lock is an OS file lock released automatically when the
holding process exits, so a crashed or killed `eph` cannot leave a later
command wedged.

## `docker compose down` failed during `eph down` or `eph clean`

A compose service's teardown failure, such as a missing `docker compose`
plugin, is a real error and stops the rest of the teardown. eph tears down by
recorded project name without rereading the Compose file:
`docker compose -p eph-<short_id>-<service> down` reproduces the command.
Fix the underlying problem and rerun `eph down` or `eph clean`. `--skip-hooks`
does not help here; it only skips lifecycle hooks.

If the workspace directory itself was deleted, run `eph system prune` from
anywhere. It scans all eph state directories and removes resources for
recorded workspace paths that are missing or empty folders. Use `eph system
prune --dry-run` first to see the plan. An 8-character state directory without
workspace metadata is skipped unless you pass `--compatibility-v042`.

For `run=` services, system prune stops only a recorded PID whose live process
still matches the identity eph captured at launch; PIDs can be reused, so a
mismatch is skipped with a warning rather than killed. A command that
deliberately detached grandchildren outside the shell tree eph launched can
still leave processes behind after that tree exits; stop those manually.

## `run=` teardown says the PID has no recorded process identity

A `run=` state entry without process identity cannot distinguish its PID from a
reused PID. `eph down`, `eph clean`, and config reconciliation refuse to signal
a live PID in that state. Inspect the process, stop it manually if it belongs to
the workspace, and rerun the eph command. Every launch must capture a stable
identity; startup stops the child and fails when capture is unavailable.

## Windows

`eph` runs natively on Linux, macOS, and Windows. Docker-backed sources use the
local Docker daemon and platform path conventions.

The shell-based features (`run=` services, lifecycle hooks, and `run`/
`compose` health checks) run through the platform shell: `sh -c` on Unix and
`cmd /C` on Windows. Process liveness and teardown are native (no POSIX
`kill` required), and teardown stops the whole process tree a `run=` command
spawned, so a compound command's children are not orphaned. The *features*
work natively on Windows with no WSL.

What does not cross over automatically is the command string itself: a `run=`,
hook, or health-check command written for `sh` (pipes, `$VAR`, `&&`, POSIX
tools) may need a `cmd`-compatible rewrite on Windows (`%VAR%`, different
builtins). Two ways to handle it:

- Write the command for `cmd`, or call a cross-platform binary directly; many,
  like `pg_isready` and `redis-cli`, take the same arguments on both
  platforms.
- Keep your POSIX command strings and run `eph` inside **WSL**, where it is a
  Linux process and `sh` is the shell.

Under WSL, `eph` is a Linux process, so its state directory is the **Linux**
path (`~/.local/share/eph/<short_id>/`), not the Windows `%LOCALAPPDATA%`
path. The `%LOCALAPPDATA%` location applies only to a native Windows build.
(`EPH_STATE_ROOT` overrides either default; see
[Core Concepts](concepts.md#persisted-state).)

### Relative bind mounts on Windows

A relative host bind mount (`volume=./seed:/docker-entrypoint-initdb.d`)
resolves against the workspace root and is passed to Docker as a plain
`C:\...` path. Absolute drive-letter paths (`C:\...`, `C:/...`) and UNC paths
are accepted. Docker does not accept an extended-length `\\?\...` bind source;
eph rejects that form before container creation. Move a workspace whose
resolved path requires that prefix to a shorter path.

## Getting more detail

`eph -v <command>` enables debug logging (to stderr). For a service's own
output, use `eph logs <service>` (add `-f` to follow). For Docker-level
issues, drop to the `docker` CLI using the names from `eph info`:

```sh
docker ps -a | grep eph-<short_id>          # containers for this workspace
docker logs eph-<short_id>-<service>        # a service's logs (or: eph logs <service>)
docker volume ls | grep eph-<short_id>      # named volumes for this workspace
```

## Next

The [Command Reference](command-reference.md) has every command and flag in
one place.
