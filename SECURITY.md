# Security Policy

`eph` is a developer tool that, by design, executes commands and manages Docker
containers based on the contents of a workspace's `.eph` file. Specifically, it:

- runs shell commands from `.eph` (`run=`, the `pre-start=` / `post-start=` /
  `pre-stop=` / `post-stop=` lifecycle hooks, and `healthcheck=`) on your machine,
- starts, stops, and removes Docker containers and named volumes,
- publishes container ports (bound to `127.0.0.1`), and
- reads and prints environment values (`eph env`) and persists service state to
  your platform's local-data directory.

Because a `.eph` file can run arbitrary commands, **treat a `.eph` file from an
untrusted source like an untrusted shell script.** Review it before running
`eph` in that workspace.

## Reporting a Vulnerability

Please report suspected vulnerabilities privately rather than opening a public
issue. Use GitHub's private vulnerability reporting:

> Repository **Security** tab -> **Report a vulnerability**
> (<https://github.com/attunehq/doteph/security/advisories/new>)

Useful reports include: command execution beyond the documented hook behavior,
ports exposed beyond `127.0.0.1`, leakage of secrets from `.eph`/environment
output, path traversal or unsafe host-path handling in volume or Dockerfile
resolution, container-name or volume-name collisions across workspaces, and
vulnerable dependencies.

There is no bug bounty program. Reports are still appreciated, and responsible
disclosure helps keep users safer. Only the current `main` development line is
supported.

## Maturity

`eph` is early-stage software and has not had a professional security audit. It
is intended for local development use. Use it at your own risk and keep your
`.eph` files and the environment values they produce private.
