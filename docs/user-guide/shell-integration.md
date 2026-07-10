---
title: "Shell Integration"
summary: "Load resolved connection details into bash, zsh, fish, JSON, direnv, and your app."
order: 6
---

# Shell Integration

Your services run on random ports, so anything that talks to them needs the
current connection details. `eph env` produces them; this page covers loading
them into bash and zsh, fish, JSON-consuming tools, your editor, and your app.

## What `eph env` prints

`eph env` prints the **top-level** environment variables from your `.eph`
file, with every `${service.property}` resolved against the **currently
running** services. It does **not** print service `env.*` values; those belong
to the containers, not your shell.

```sh
$ eph up
$ eph env
export DATABASE_URL="postgres://dev:dev@localhost:54321/myapp"
export REDIS_URL="redis://localhost:54322"
```

Output goes to **stdout** and all logs go to stderr, which keeps the output
clean for `eval` and piping.

> Run `eph up` before `eph env`. Interpolation only resolves for running
> services. If a variable still references a stopped service, shell formats
> unset that variable and finish with a failing shell statement. This clears a
> value left by another workspace and makes `eval "$(eph env)"` return nonzero.
> JSON omits the unresolved variable. The `eph env` process also exits nonzero
> in every format and reports the missing reference on stderr.

## Formats

Choose with `-f` / `--format`. The default is `export`.

```sh
eph env                     # export (bash/zsh/sh), the default
eph env -f fish             # fish
eph env -f powershell       # PowerShell
eph env -f json             # JSON object
```

### bash / zsh / sh

```sh
eval "$(eph env)"
```

### fish

fish needs the output collected so newlines are preserved:

```fish
eph env -f fish | source
# or, equivalently:
eval (eph env -f fish | string collect)
```

### PowerShell

```powershell
eph env --format powershell | Out-String | Invoke-Expression
```

`Out-String` collects the lines into one string before `Invoke-Expression`
runs it, the PowerShell equivalent of fish's `| source`. Each line is
`$env:NAME = 'value'`, with an embedded `'` doubled per PowerShell's own
single-quoted-string escaping rule.

### JSON

For tools, scripts, and agents that would rather parse structured data than
shell syntax:

```sh
$ eph env -f json
{
  "DATABASE_URL": "postgres://dev:dev@localhost:54321/myapp",
  "REDIS_URL": "redis://localhost:54322"
}
```

Pipe it into `jq`, a `.env` writer, or your own tooling:

```sh
# Pull a single value without hiding eph's exit status in a pipeline
eph_json="$(eph env -f json)" && jq -r .DATABASE_URL <<<"$eph_json"

# Write a .env file for tools that read one
eph_json="$(eph env -f json)" &&
  jq -r 'to_entries[] | "\(.key)=\(.value)"' <<<"$eph_json" > .env.local
```

## Skipping the shell entirely

Two features hand the resolved environment to a process directly, no `eval`
required:

- [`eph run <cmd>`](command-reference.md#eph-run-cmd) runs any command with
  the resolved variables (plus `EPH_*` metadata) set:
  `eph run ./scripts/seed.sh`. It refuses to start the command if any required
  service reference is unresolved.
- A [`run=` service](run-your-app.md#the-app-is-a-run-service) inherits the
  same environment at launch, because eph is the one launching it.

Reach for the shell integration on this page when the consumer is your
interactive shell or a tool eph does not launch.

## Escaping

Values are escaped for the target shell:

- **export** and **fish** emit the value inside double quotes: export escapes
  backslash, `"`, `$`, and backtick; fish escapes backslash, `"`, and `$`
  (fish does not treat backticks specially inside double quotes).
- **powershell** emits the value inside single quotes (`$env:NAME = 'value'`),
  PowerShell's literal-string form: nothing is interpolated inside it, so the
  only character that needs escaping is the single quote itself, doubled
  (`it's` becomes `it''s`).

Literal newlines inside a value are preserved. You do not need to quote values
in your `.eph` file for the shell's benefit; `eph` handles escaping.

## Auto-loading per directory

You usually want the variables loaded whenever you enter a project.

### direnv

Add an `.envrc` to the project:

```sh
# .envrc
if [ -f .eph ]; then
  eval "$(eph env)"
fi
```

Then `direnv allow`. direnv loads the variables when you `cd` in and unloads
them when you `cd` out. If you want services started too, add `eph up` above
the `eval`, but note that checking containers on every `cd` can be slow; most
people prefer an explicit `eph up`.

### A shell function

Drop this in your shell rc to start and load in one step:

```sh
# bash / zsh
ephup() { eph up && eval "$(eph env)"; }
```

```fish
function ephup
    eph up; and eph env -f fish | source
end
```

## Using it in your app

Anything that reads environment variables works once you have run
`eval "$(eph env)"`, and an app managed as a `run=` service gets the variables
without even that. If your framework insists on a `.env` file, generate one
from the JSON output (see above); regenerate it after `eph up` assigns new
ports, and keep generated files out of version control.

## Next

[Recipes](recipes.md) puts all of this together: Compose migration, seeding,
CI, prewarming for agents, and secrets.
