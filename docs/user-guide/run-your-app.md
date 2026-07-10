---
title: "Running Your App"
summary: "Run your own app under eph: port=auto, eph dev, watch mode, and preview servers."
order: 5
---

# Running Your App

The backing services are only half the workspace. The other half is the app
you are actually building: the dev server you restart constantly, the process
that needs `DATABASE_URL` set and a port to bind. This page brings it under
eph's management, which buys you four things:

- **A collision-free port.** Two checkouts of the same project stop fighting
  over port 3000.
- **The environment, injected.** The app inherits every resolved variable
  (`DATABASE_URL` and friends) at launch. No `eval`, no `.env` shuffle.
- **Ordering.** Backing services come up healthy before the app starts.
- **One foreground command.** `eph dev` runs the whole stack (setup, seeding,
  the app, teardown) as a single process, which is exactly what preview
  servers and simple task runners want.

## The app is a `run=` service

Model the app as a [`run=` service](services.md#run-a-process-instead-of-a-container)
with `port=auto`:

```ini
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_PASSWORD=dev
env.POSTGRES_DB=myapp
healthcheck=pg_isready -U dev

[web]
run=npm run dev
port=auto
env.PORT=${web.port}

[env]
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/myapp
APP_URL=http://localhost:${web.port}
```

Because eph launches the process, `npm run dev` starts with `DATABASE_URL`
already set (backing services start first, so the value resolves), plus the
`EPH_*` metadata and its own `env.*` values. The app connects to Postgres with
zero shell plumbing.

## `port=auto`

Container services get their host port from Docker. A `run=` process binds its
own port, so eph handles it the other way around: `port=auto` makes eph
allocate a free host port and hand it to the process through its environment.

```ini
[web]
run=npm run dev
port=auto
# tell your framework which port to bind
env.PORT=${web.port}
```

How it behaves:

- **eph picks the port and injects it.** Reference the service's own assigned
  port as `${web.port}` in its `env.*`; that is how the value reaches the
  process (most frameworks read `PORT`). The same port resolves everywhere
  else: other services' hooks, `eph env`, `APP_URL` above.
- **Stable across restarts.** eph reuses the previously assigned port on the
  next `eph up` when it is still free, so bookmarks and OAuth callback URLs
  keep working. It only moves when the old port is taken.
- **Self-healing on conflict.** There is an unavoidable instant between eph
  reserving a port and your process binding it. Because eph owns the launch,
  it watches for an early exit that looks like a port conflict ("address
  already in use") and relaunches on a fresh port automatically, a few times
  before giving up.
- **Use your framework's strict-port mode.** The self-heal works only if your
  dev server *exits* on a busy port. A framework that silently picks the next
  port instead (Vite without `--strictPort`, for example) sidesteps both the
  conflict detection and the port eph reports. Strict mode makes the assigned
  port the bound port.
- `port=auto` is only valid for `run=` services; container services already
  get a random host port from Docker.

Multiple auto ports work too (a frontend plus its HMR socket):

```ini
[web]
run=npm run dev
port.app=auto
port.hmr=auto
env.PORT=${web.port.app}
env.VITE_HMR_PORT=${web.port.hmr}
```

## `eph dev`: the foreground loop

`eph up` starts services in the background and returns. `eph dev` runs the
same stack as one foreground process.

```sh
eph dev            # foreground the sole run= service
eph dev web        # foreground a specific run= service by name
```

What it does, in order:

1. Runs every service's `pre-start` hooks up front.
2. Brings the backing services up and waits for health.
3. Starts the chosen `run=` app in the **foreground**: eph's stdin, stdout,
   and stderr are wired straight through, so the app is fully interactive and
   its output streams to your terminal. eph's own startup chrome goes to
   stderr, out of the app's stdout.
4. Runs every service's `post-start` hooks (migrations, seeds).
5. Blocks until the app is stopped, then tears down **only the services it
   started itself**, and leaves anything that was already running (a
   prewarmed dependency tier, say) untouched.

The teardown default is `eph down`: containers and data stay for a fast next
launch. Pass `--clean` to end with a full `eph clean` instead, dropping named
volumes and their data for a pristine next run.

If the app crashes, `eph dev` exits non-zero and leaves the backing services
up; `eph down` stops them when you are done. A hard kill of eph itself
(`SIGKILL`) cannot run teardown, so the stack stays up, recoverable with
`eph down`.

With no `SERVICE` argument, `eph dev` foregrounds the sole `run=` service in
the file; name one when there are several. A `.eph` with no `run=` service is
an error.

## Watch mode

`--watch GLOB` (repeatable) restarts the whole stack when a matching file
changes:

```sh
eph dev --watch "**/*.rs" --watch "*.toml"
```

- Globs are relative to the workspace root with gitignore-style separators:
  `*` stays within a directory, `**` spans directories, so `*.toml` matches a
  top-level `Cargo.toml` while `**/*.rs` matches any Rust file.
- A restart is a full down and up, hooks included (`pre-stop`, `post-stop`,
  `pre-start`, `post-start`), not a bare process bounce, so a migration or
  codegen step reruns just as it would on a manual restart. Named volumes are
  always kept across restarts, even under `--clean`; that reset is reserved
  for the final stop.
- Only the services `eph dev` started are bounced. An adopted, already-running
  dependency tier stays hot across restarts.
- Changes are debounced (one save is one restart), and git's churn under
  `.git` never triggers one.
- In watch mode a crashing app does not end the session: eph reports the exit
  and waits for your next change to restart, since editing is exactly when
  crashes happen. Without `--watch`, the same crash ends the command.

## Claude Desktop preview servers

[Claude Desktop](https://code.claude.com/docs/en/desktop#configure-preview-servers)
launches a dev server from `.claude/launch.json` and watches its port for the
in-app preview. Each configuration runs a single foreground command with no
separate setup or teardown hook, which is exactly the shape of `eph dev`: one
command that brings the stack up, seeds it, foregrounds the app, and tears
down when stopped.

Point the preview at `eph dev`:

```jsonc
// .claude/launch.json
{
  "version": "0.0.1",
  "configurations": [
    {
      "name": "web",
      "runtimeExecutable": "eph",
      "runtimeArgs": ["dev"],
      "port": 3000,
      "autoPort": true
    }
  ]
}
```

With a `.eph` like the one at the top of this page (postgres, plus a `web`
app with `port=auto` and a `post-start=npm run db:migrate` seed), the pieces
line up like this:

- **`autoPort` hands eph the port.** The preview server picks a free host
  port and passes it as `$PORT`, then polls that port and reveals the app the
  instant it accepts a connection. When `$PORT` is set, `eph dev` keeps the
  app on its own internal `port=auto` and opens `$PORT` as a forwarding gate
  to it. Do not also pin a fixed port on the app.
- **The gate waits for seeding, not just for the server.** `eph dev` opens
  `$PORT` only *after* `post-start` hooks finish. The preview therefore cannot
  go live while a slow seed is still filling the database; without the gate it
  would show an empty app the moment the server answered its first request.
- **The preview console is live.** The app's output streams through eph to
  the preview console, and stdin flows back.
- **Restarts are cheap.** Claude Desktop stops and relaunches the preview
  server during a session, and the default `eph down` teardown keeps the
  database between launches. Use `runtimeArgs: ["dev", "--clean"]` only if you
  want a pristine database every launch.
- **`eph` must be on the app's PATH.** A desktop app does not always inherit
  your shell `PATH` (notably a macOS Dock launch); use an absolute path in
  `runtimeExecutable` if needed.

If you would rather keep the app out of eph, model only the backing services
in `.eph` and let `launch.json` run the app through
[`eph run`](command-reference.md#eph-run-cmd) so it still gets the resolved
environment: `"runtimeExecutable": "eph", "runtimeArgs": ["run", "npm", "run",
"dev"]`. You then own `eph up` and `eph down` yourself; that manual work is
what `eph dev` automates.

## Splitting the app from its dependencies

Once your file has both tiers, [roles](eph-file.md#roles-and-ordering) make
them addressable: tag services `role=dep` and `role=app`, declare
`roles_order=dep,app`, and you can prewarm the slow dependency tier once
(`eph up --role dep`) while `eph dev` starts and stops only the app. `eph dev`
adopts an already-running tier rather than restarting it, and leaves it
running on exit. The full workflow, including a Claude Code SessionStart hook,
is in [Recipes](recipes.md#prewarm-dependency-services-on-claude-code-session-start).

## Next

[Shell Integration](shell-integration.md) covers getting the resolved
environment into your shell and tools when the app runs outside eph.
