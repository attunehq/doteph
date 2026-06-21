# Shell Integration

Your services run on random ports, so your app needs the current connection
details. `eph env` produces them; this page covers loading them into bash/zsh,
fish, your app, an editor, or an agent.

## What `eph env` prints

`eph env` prints the **top-level** environment variables from your `.eph` file,
with every `${service.property}` resolved against the **currently running**
services. It does **not** print service `env.*` values - those belong to the
containers, not your shell.

```sh
$ eph up
$ eph env
export DATABASE_URL="postgres://dev:dev@localhost:54321/myapp"
export REDIS_URL="redis://localhost:54322"
```

Output goes to **stdout**; all logs go to stderr. That keeps the output clean
for `eval` and piping.

> Run `eph up` before `eph env`. Interpolation only resolves for running
> services; placeholders for stopped services are left unresolved (literally
> `${name.port}`) rather than blanked.

## Formats

Choose with `-f` / `--format`. The default is `export`.

```sh
eph env                # export (bash/zsh/sh) - the default
eph env -f fish        # fish
eph env -f json        # JSON object
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

### JSON

For tools, scripts, and agents that would rather parse structured data than
shell:

```sh
$ eph env -f json
{
  "DATABASE_URL": "postgres://dev:dev@localhost:54321/myapp",
  "REDIS_URL": "redis://localhost:54322"
}
```

Pipe it into `jq`, a `.env` writer, or your own tooling:

```sh
# Pull a single value
eph env -f json | jq -r .DATABASE_URL

# Write a .env file for tools that read one
eph env -f json | jq -r 'to_entries[] | "\(.key)=\(.value)"' > .env.local
```

## Escaping

Values are emitted inside double quotes and escaped for the target shell:

- **export**: backslash, `"`, `$`, and backtick are escaped.
- **fish**: backslash, `"`, and `$` are escaped (fish does not treat backticks
  specially in double quotes).

Literal newlines inside a value are preserved. You do not need to quote values
in your `.eph` file for the shell's benefit - `eph` handles escaping.

## Auto-loading per directory

You usually want the variables loaded whenever you enter a project. A few
options:

### direnv

Add an `.envrc` to the project:

```sh
# .envrc
if [ -f .eph ]; then
  eval "$(eph env)"
fi
```

Then `direnv allow`. direnv loads the variables on `cd` in and unloads them on
`cd` out. (If you want services started too, add `eph up` above the `eval` - but
note that starting containers on every `cd` can be slow; many people prefer to
`eph up` explicitly.)

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

Anything that reads environment variables works once you have run `eval "$(eph
env)"`. If your framework reads a `.env` file instead, generate one from the
JSON output (see above) - just remember to regenerate it after `eph up` assigns
new ports, and keep generated files out of version control.

## Next

See [Recipes](recipes.md) for CI, Compose migration, and multi-checkout
workflows, or the [Command Reference](command-reference.md) for the full `env`
specification.
