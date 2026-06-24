//! Command-line front end for `eph`.
//!
//! This binary is a thin [`clap`] layer over the [`eph`] library: it defines the
//! CLI ([`Cli`] / [`Commands`]), initializes logging, and dispatches each
//! subcommand to a small `cmd_*` glue function that calls into
//! [`eph::ServiceManager`], [`eph::Workspace`], and the parser/env APIs. All the
//! reusable logic lives in the library; nothing here is part of the public API.

#![deny(clippy::correctness)]
#![warn(clippy::suspicious)]
#![warn(clippy::style)]
#![warn(clippy::complexity)]
#![warn(clippy::perf)]

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use eph::parser::{self, EphFile, ServiceSource};
use eph::{LogOptions, RunningService, ServiceManager, Workspace, skills};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, ExitCode};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "eph")]
#[command(about = "Ephemeral services per workspace - dotenv for services")]
#[command(version = env!("EPH_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start all services defined in .eph
    Up {
        /// Specific services to start (defaults to all)
        #[arg(value_name = "SERVICE")]
        services: Vec<String>,

        /// Bring services up healthy but do not run their post-start hooks
        #[arg(long = "skip-hooks")]
        skip_hooks: bool,
    },

    /// Stop all services
    Down {
        /// Specific services to stop (defaults to all)
        #[arg(value_name = "SERVICE")]
        services: Vec<String>,

        /// Remove containers after stopping them (instead of just stopping)
        #[arg(short = 'r', long = "rm")]
        rm: bool,

        /// Stop services without running their pre-stop hooks
        #[arg(long = "skip-hooks")]
        skip_hooks: bool,
    },

    /// Stop and remove all services, named volumes, and persisted state
    Clean {
        /// Tear everything down without running pre-stop hooks
        #[arg(long = "skip-hooks")]
        skip_hooks: bool,
    },

    /// Show status of services
    Status,

    /// Print environment variables for shell eval
    /// Usage: eval "$(eph env)"
    Env {
        /// Output format: export (default), fish, json
        #[arg(short, long, default_value = "export")]
        format: String,
    },

    /// Run a command with eph's resolved environment.
    ///
    /// The command runs in the workspace root with the same connection
    /// variables `eph env` emits, plus `EPH_*` metadata, so you can do
    /// `eph run psql "$DATABASE_URL"` or `eph run ./scripts/seed.sh` without
    /// `eval`-ing anything first. The command is executed directly, not through
    /// a shell; use `eph run sh -c '...'` if you need shell features.
    Run {
        /// The command and its arguments.
        #[arg(
            value_name = "CMD",
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<String>,
    },

    /// Show logs for services.
    ///
    /// With a SERVICE, passes that service's logs through raw; without one,
    /// streams every service interleaved with each line tagged `[name]` (like
    /// `docker compose logs`). `run=` services read from their captured log file;
    /// `image=` / `dockerfile=` / `compose=` services proxy `docker logs` /
    /// `docker compose logs`. Logs are shown even for a stopped service, so a
    /// `run=` service that died on startup still leaves a trace.
    Logs {
        /// Service to show logs for (defaults to every defined service).
        #[arg(value_name = "SERVICE")]
        service: Option<String>,

        /// Follow log output, like `tail -f` (Ctrl-C to stop).
        #[arg(short = 'f', long)]
        follow: bool,

        /// Show only the last N lines.
        #[arg(short = 'n', long, value_name = "N")]
        tail: Option<usize>,
    },

    /// Parse and validate .eph file
    Check,

    /// Print workspace info
    Info,

    /// Manage the bundled agent skills that teach coding agents to use eph.
    Skills {
        /// The skills subcommand to run.
        #[command(subcommand)]
        command: SkillsCommand,
    },
}

/// Skills subcommands. They install the skills bundled into this binary into a
/// repository (so its agents discover how to drive `eph up` / `eph env`) and
/// check that the checked-in copies are still current.
#[derive(Subcommand)]
enum SkillsCommand {
    /// Write the bundled skills into the repository so agents discover them.
    Install {
        /// Skills directory to install into, relative to the repo root. Repeatable;
        /// defaults to `.claude/skills` and `.agents/skills`.
        #[arg(long = "dir", value_name = "DIR")]
        dirs: Vec<PathBuf>,
        /// Overwrite existing skill files even if they were edited locally.
        #[arg(long)]
        force: bool,
    },
    /// Check that the installed skills match this binary (non-zero exit on drift).
    Check {
        /// Skills directory to check, relative to the repo root. Repeatable;
        /// defaults to `.claude/skills` and `.agents/skills`.
        #[arg(long = "dir", value_name = "DIR")]
        dirs: Vec<PathBuf>,
    },
    /// List the skills bundled into this binary.
    List,
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();

    // Initialize logging
    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };
    // Log to stderr, never stdout. stdout carries the command's real output
    // (e.g. `eph env` emits shell/JSON meant for `eval "$(eph env)"` or piping
    // into a parser); mixing log lines into it corrupts that machine-readable
    // output.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    match cli.command {
        Commands::Up {
            services,
            skip_hooks,
        } => cmd_up(services, skip_hooks)
            .await
            .map(|()| ExitCode::SUCCESS),
        Commands::Down {
            services,
            rm,
            skip_hooks,
        } => cmd_down(services, rm, skip_hooks)
            .await
            .map(|()| ExitCode::SUCCESS),
        Commands::Clean { skip_hooks } => cmd_clean(skip_hooks).await.map(|()| ExitCode::SUCCESS),
        Commands::Status => cmd_status().await.map(|()| ExitCode::SUCCESS),
        Commands::Env { format } => cmd_env(&format).await.map(|()| ExitCode::SUCCESS),
        Commands::Run { command } => cmd_run(command).await,
        Commands::Logs {
            service,
            follow,
            tail,
        } => cmd_logs(service, follow, tail)
            .await
            .map(|()| ExitCode::SUCCESS),
        Commands::Check => cmd_check().await.map(|()| ExitCode::SUCCESS),
        Commands::Info => cmd_info().await.map(|()| ExitCode::SUCCESS),
        // Skills commands are synchronous filesystem work; they do not touch
        // Docker or the running tokio runtime.
        Commands::Skills { command } => match command {
            SkillsCommand::Install { dirs, force } => {
                cmd_skills_install(&dirs, force).map(|()| ExitCode::SUCCESS)
            }
            // Drifted or missing skills are a fail-closed signal for CI, so map
            // them to a non-zero exit without raising an error.
            SkillsCommand::Check { dirs } => cmd_skills_check(&dirs).map(|current| {
                if current {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }),
            SkillsCommand::List => cmd_skills_list().map(|()| ExitCode::SUCCESS),
        },
    }
}

async fn cmd_up(service_filter: Vec<String>, skip_hooks: bool) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let mut manager = ServiceManager::new(workspace).await?;

    let running = manager
        .start_services(&eph, &service_filter, skip_hooks)
        .await?;

    // Print summary in declaration order (iterate the .eph definitions rather
    // than the unordered `running` map) so the output is reproducible.
    println!();
    println!("Services started:");
    for name in eph.services.keys() {
        if let Some(svc) = running.get(name) {
            print_service_ports(name, svc);
        }
    }

    // Print environment hint
    println!();
    println!("Run `eval \"$(eph env)\"` to set environment variables");

    Ok(())
}

async fn cmd_down(service_filter: Vec<String>, rm: bool, skip_hooks: bool) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let mut manager = ServiceManager::new(workspace).await?;

    let action = if rm { "stopped and removed" } else { "stopped" };

    if service_filter.is_empty() {
        manager.stop_all(&eph, rm, skip_hooks).await?;
        println!("All services {}", action);
    } else {
        // Snapshot running services once so pre-stop hooks see the full
        // environment as it was before teardown began.
        let running = manager.status().await?;
        for name in &service_filter {
            let service = eph
                .services
                .get(name)
                .with_context(|| format!("unknown service: {}", name))?;
            manager
                .stop_service(name, service, rm, &eph, &running, skip_hooks)
                .await?;
            println!("{} {}", if rm { "Removed" } else { "Stopped" }, name);
        }
        // Persist so the stopped services are dropped from state.json, not just
        // from the in-memory copy. stop_all already saves; a targeted down must
        // too, or the file keeps stale entries until the next `eph status`.
        manager.save_state().await?;
    }

    Ok(())
}

async fn cmd_clean(skip_hooks: bool) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let mut manager = ServiceManager::new(workspace).await?;
    let summary = manager.clean(&eph, skip_hooks).await?;

    println!("Workspace cleaned:");
    println!(
        "  Services stopped and removed: {}",
        summary.services_removed
    );
    println!("  Named volumes removed: {}", summary.volumes_removed);
    println!(
        "  Persisted state: {}",
        if summary.state_removed {
            "removed"
        } else {
            "none"
        }
    );

    Ok(())
}

async fn cmd_status() -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    // Print workspace header before moving `workspace` into the manager.
    println!("Workspace: {}", workspace.path.display());
    println!("ID: {}", workspace.short_id);
    println!();

    let manager = ServiceManager::new(workspace).await?;
    let running = manager.status().await?;

    if running.is_empty() {
        println!("No services running");
        println!();
        println!("Defined services:");
        for name in eph.services.keys() {
            println!("  {} (stopped)", name);
        }
    } else {
        println!("Running services:");
        // Declared services first, in declaration order, so the listing is
        // reproducible across runs.
        for name in eph.services.keys() {
            if let Some(svc) = running.get(name) {
                print_service_ports(name, svc);
            }
        }
        // Then any service that is running in persisted state but no longer
        // declared in the `.eph` file (renamed or removed). These are still
        // surfaced so they remain visible and tearable, sorted by name to stay
        // deterministic.
        let mut undeclared: Vec<&String> = running
            .keys()
            .filter(|n| !eph.services.contains_key(*n))
            .collect();
        undeclared.sort();
        for name in undeclared {
            if let Some(svc) = running.get(name) {
                print_service_ports(name, svc);
            }
        }

        let stopped: Vec<_> = eph
            .services
            .keys()
            .filter(|n| !running.contains_key(*n))
            .collect();
        if !stopped.is_empty() {
            println!();
            println!("Stopped services:");
            for name in stopped {
                println!("  {}", name);
            }
        }
    }

    Ok(())
}

/// Print a running service and its assigned host ports for `eph up` / `eph
/// status`. A single port is shown inline; multiple named ports (e.g. an app
/// with both a frontend and an HMR port) are listed one per line. Names are
/// sorted so the output is stable across runs regardless of map order.
fn print_service_ports(name: &str, svc: &RunningService) {
    let mut ports: Vec<(&String, &u16)> = svc.ports.iter().collect();
    ports.sort_by(|a, b| a.0.cmp(b.0));

    match ports.as_slice() {
        [] => println!("  {} (no ports)", name),
        [(_, port)] => println!("  {} -> localhost:{}", name, port),
        many => {
            println!("  {}:", name);
            for (port_name, port) in many {
                println!("    {} -> localhost:{}", port_name, port);
            }
        }
    }
}

async fn cmd_env(format: &str) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let manager = ServiceManager::new(workspace).await?;
    let running = manager.status().await?;

    // The resolved KEY=VALUE pairs, shared with the lifecycle-hook and `eph run`
    // machinery so a developer's shell and a post-start hook see the same env.
    let env_vars = eph::resolve_env_vars(&eph, &running);

    // Render in the requested format
    print!("{}", eph::render(&env_vars, format)?);

    Ok(())
}

/// Run an arbitrary command with eph's resolved environment overlaid on the
/// current process environment, returning the command's own exit code.
async fn cmd_run(command: Vec<String>) -> Result<ExitCode> {
    let workspace = Workspace::find_from_cwd()?;
    let workspace_root = workspace.path.clone();
    let eph = load_eph_file(&workspace)?;

    let manager = ServiceManager::new(workspace).await?;
    let running = manager.status().await?;
    let env = manager.command_env(&eph, &running);

    // `required = true` on the arg guarantees a non-empty vector.
    let (program, rest) = command
        .split_first()
        .expect("clap guarantees at least one argument");

    // Inherit eph's stdio so the command is fully interactive, and run it from
    // the workspace root so relative paths resolve the way hooks do.
    let status = StdCommand::new(program)
        .args(rest)
        .current_dir(&workspace_root)
        .envs(env)
        .status()
        .with_context(|| format!("failed to run command: {}", program))?;

    // Propagate the child's exit code. A process killed by a signal has no
    // code; report the conventional 128 + signal-style failure as 1.
    let code = status.code().unwrap_or(1);
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

/// Show logs for one service (raw), or every service interleaved and tagged.
async fn cmd_logs(service: Option<String>, follow: bool, tail: Option<usize>) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;
    let manager = ServiceManager::new(workspace).await?;
    let opts = LogOptions { follow, tail };

    // A single named service passes its raw stream straight through (untagged,
    // pipe-friendly, follow-capable).
    if let Some(name) = service {
        return manager.logs(&eph, &name, &opts).await;
    }

    // No service: stream every service interleaved, prefixing each line with a
    // `[name]` tag the way `docker compose logs` does. Sort the names so tag
    // colors and column width are stable across runs regardless of map order.
    let mut names: Vec<String> = eph.services.keys().cloned().collect();
    names.sort();

    // Right-align every tag to the widest one so the log text lines up, and color
    // the tag per service (deterministically from its name) on a terminal.
    let tag_width = names
        .iter()
        .map(|n| n.chars().count() + 2) // +2 for the surrounding brackets
        .max()
        .unwrap_or(0);
    let colorize = should_colorize();
    let prefixes: HashMap<String, String> = names
        .iter()
        .map(|name| {
            let tag = format!("[{}]", name);
            let pad = " ".repeat(tag_width.saturating_sub(tag.chars().count()));
            let prefix = if colorize {
                format!("{pad}\x1b[{}m{}\x1b[0m", tag_color(name), tag)
            } else {
                format!("{pad}{tag}")
            };
            (name.clone(), prefix)
        })
        .collect();

    // Hold the stdout lock for the whole stream: lines arrive from concurrent
    // tasks, but writing them here (one consumer) keeps each line atomic. A
    // write error (closed pipe, e.g. `eph logs | head`) ends the stream quietly
    // rather than panicking the way `println!` would.
    let stdout = io::stdout();
    let mut out = stdout.lock();
    manager
        .stream_logs(&eph, &names, &opts, |service, line| {
            let prefix = prefixes.get(service).map_or(service, String::as_str);
            writeln!(out, "{prefix} {line}")
        })
        .await
}

/// ANSI SGR foreground codes used to color `eph logs` service tags. Theme-aware
/// terminal palette colors (rather than fixed RGB) so they stay legible on both
/// light and dark backgrounds. Red is omitted to avoid an error connotation.
const TAG_COLORS: &[u8] = &[36, 33, 32, 35, 34, 96, 93, 92, 95, 94];

/// Pick a stable color for a service tag by hashing its name (FNV-1a) into
/// [`TAG_COLORS`], so a given service always gets the same color.
fn tag_color(name: &str) -> u8 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    TAG_COLORS[(hash as usize) % TAG_COLORS.len()]
}

/// Whether to emit ANSI color: only when stdout is a terminal and the caller has
/// not opted out via the `NO_COLOR` convention. Keeps piped/redirected output
/// (`eph logs | grep`, `eph logs > file`) clean.
fn should_colorize() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

async fn cmd_check() -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    println!("Valid .eph file: {}", workspace.eph_file_path().display());
    println!();
    println!("Environment variables: {}", eph.env_vars.len());
    for var in &eph.env_vars {
        println!("  {}", var.name);
    }
    println!();
    println!("Services: {}", eph.services.len());
    for (name, svc) in &eph.services {
        let source = match &svc.source {
            ServiceSource::Image(img) => format!("image: {}", img),
            ServiceSource::Dockerfile(path) => format!("dockerfile: {}", path),
            ServiceSource::Compose(path) => format!("compose: {}", path),
            ServiceSource::Command(cmd) => format!("command: {}", cmd),
        };
        println!("  {} ({})", name, source);
    }

    Ok(())
}

async fn cmd_info() -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;

    println!("Workspace path: {}", workspace.path.display());
    println!("Workspace ID: {}", workspace.id);
    println!("Short ID: {}", workspace.short_id);
    println!("Container prefix: {}", workspace.container_prefix());
    println!(".eph file: {}", workspace.eph_file_path().display());
    println!("State directory: {}", workspace.state_dir()?.display());

    Ok(())
}

/// `eph skills install`: write the bundled agent skills into the repository.
///
/// Resolves the repository root from the current directory, writes each bundled
/// skill into every target directory (the defaults, or the `--dir` overrides),
/// and prints what it did. Existing files that differ are left untouched unless
/// `force` is set, so a local edit is never clobbered silently.
fn cmd_skills_install(dirs: &[PathBuf], force: bool) -> Result<()> {
    let root = skills_root()?;
    let targets = resolve_skill_dirs(dirs);
    let outcomes = skills::install(&root, &targets, force)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut skipped = 0usize;
    for outcome in &outcomes {
        let label = match outcome.status {
            skills::Installed::Created => "created",
            skills::Installed::Updated => "updated",
            skills::Installed::Unchanged => "unchanged",
            skills::Installed::Skipped => {
                skipped += 1;
                "skipped (exists)"
            }
        };
        writeln!(out, "  {label}: {}", relative_to(&root, &outcome.path))?;
    }
    if skipped > 0 {
        writeln!(
            out,
            "\n{skipped} file(s) already existed and were left as-is; re-run with --force to overwrite."
        )?;
    } else {
        writeln!(
            out,
            "\nCommit these files so your agents discover them on checkout."
        )?;
    }
    Ok(())
}

/// `eph skills check`: verify the installed skills match this binary's embedded
/// source.
///
/// Prints one line per skill file and returns whether every one is up to date.
/// Returns `Ok(false)` when any file is missing or has drifted (a hand edit, or a
/// stale install left behind after the skill source changed), so the caller can
/// exit non-zero: a CI step can run this to fail when the checked-in skills fall
/// out of sync with the source.
fn cmd_skills_check(dirs: &[PathBuf]) -> Result<bool> {
    let root = skills_root()?;
    let targets = resolve_skill_dirs(dirs);
    let outcomes = skills::check(&root, &targets)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut current = true;
    for outcome in &outcomes {
        let label = match outcome.status {
            skills::Checked::UpToDate => "up to date",
            skills::Checked::Drifted => {
                current = false;
                "drifted"
            }
            skills::Checked::Missing => {
                current = false;
                "missing"
            }
        };
        writeln!(out, "  {label}: {}", relative_to(&root, &outcome.path))?;
    }
    if !current {
        writeln!(
            out,
            "\nChecked-in skills are out of sync; run `eph skills install` to refresh them."
        )?;
    }
    Ok(current)
}

/// `eph skills list`: show the skills bundled into this binary.
fn cmd_skills_list() -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "Skills bundled in eph {}:", env!("EPH_VERSION"))?;
    for skill in skills::BUNDLED {
        writeln!(out, "  {} - {}", skill.slug, skill.summary)?;
    }
    writeln!(
        out,
        "\nInstall them with `eph skills install` (default targets: {}).",
        skills::DEFAULT_DIRS.join(", ")
    )?;
    Ok(())
}

/// The repository root to install skills into: the git toplevel containing the
/// current directory, or the current directory itself when it is not inside a
/// repo, so first-time setup still works.
fn skills_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("determining the current directory")?;
    Ok(git_repo_root(&cwd).unwrap_or(cwd))
}

/// The git toplevel containing `cwd`, or `None` when `cwd` is not inside a git
/// repository (or `git` is unavailable).
fn git_repo_root(cwd: &Path) -> Option<PathBuf> {
    let output = StdCommand::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    if root.is_empty() {
        return None;
    }
    Some(PathBuf::from(root))
}

/// The requested skill directories, falling back to the documented defaults when
/// none were passed.
fn resolve_skill_dirs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    if dirs.is_empty() {
        skills::default_dirs()
    } else {
        dirs.to_vec()
    }
}

/// Display `path` relative to `root` for tidy output, falling back to the full
/// path when it lies outside `root`. Separators are normalized to `/` so the
/// output is consistent across platforms and matches the docs, rather than mixing
/// the `/` in a default like `.claude/skills` with Windows' `\`.
fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn load_eph_file(workspace: &Workspace) -> Result<EphFile> {
    let path = workspace.eph_file_path();
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parser::parse(&contents).with_context(|| format!("failed to parse {}", path.display()))
}
