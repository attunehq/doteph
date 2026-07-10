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
use eph::{
    Hooks, LogOptions, PruneOptions, PruneReport, RunningService, ServiceManager, Workspace, skills,
};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, ExitCode};
use tracing_subscriber::EnvFilter;

mod watch;
use watch::Watch;

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

        /// Bring up only these roles and everything they depend on. Repeatable
        /// (`--role dep --role app`). Requires a `roles_order` in the `.eph` file.
        /// Combines with any SERVICE names (their union is started).
        #[arg(long = "role", value_name = "ROLE")]
        roles: Vec<String>,

        /// Bring services up healthy but do not run their post-start hooks
        #[arg(long = "skip-hooks")]
        skip_hooks: bool,
    },

    /// Stop all services
    Down {
        /// Specific services to stop (defaults to all)
        #[arg(value_name = "SERVICE")]
        services: Vec<String>,

        /// Stop only these roles and everything that depends on them. Repeatable
        /// (`--role dep`). Requires a `roles_order` in the `.eph` file. Combines
        /// with any SERVICE names (their union is stopped).
        #[arg(long = "role", value_name = "ROLE")]
        roles: Vec<String>,

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

    /// Manage eph's global state and resources.
    System {
        /// The system subcommand to run.
        #[command(subcommand)]
        command: SystemCommand,
    },

    /// Run the dev stack in the foreground for a Claude Desktop preview server.
    ///
    /// Brings every service up (running `post-start` hooks, e.g. seeding), then
    /// foregrounds a `run=` service with eph's own stdin, stdout, and stderr
    /// wired through to it, staying attached until it is stopped. On stop it tears
    /// the stack down: `eph down` by default (keeps containers and volume data for
    /// a fast restart), or `eph clean` with `--clean` (a full reset that also
    /// drops the named-volume data).
    ///
    /// It is built for `.claude/launch.json`, whose preview configuration runs a
    /// single foreground command and offers no separate setup or teardown hook:
    /// point `runtimeExecutable` at `eph` with `runtimeArgs` of `["dev"]`. When
    /// the preview server assigns the host port and passes it as `$PORT`, `eph
    /// dev` opens that port only after `post-start` hooks finish and forwards it to
    /// the app, so the preview (which watches the port) does not go live until
    /// seeding is done rather than the instant the server can serve a health check.
    ///
    /// With no SERVICE the sole `run=` service is foregrounded; name one
    /// explicitly when the `.eph` file defines more than one.
    ///
    /// Pass `--watch <glob>` (repeatable) to restart the whole stack when a
    /// matching file changes: `eph dev --watch "**/*.rs" --watch "*.toml"`.
    Dev {
        /// The `run=` service to foreground (defaults to the only one).
        #[arg(value_name = "SERVICE")]
        service: Option<String>,

        /// Tear down with `eph clean` (drop volumes and their data) instead of
        /// the default `eph down` (keep them for a fast restart).
        #[arg(long)]
        clean: bool,

        /// Restart the whole dev stack when a file matching GLOB changes.
        ///
        /// Repeatable; each value is a glob relative to the workspace root, with
        /// gitignore-style separators (`*` stays within a directory, `**` spans
        /// them): `--watch "**/*.rs" --watch "*.toml"`. On a change eph tears the
        /// stack down (pre-stop hooks and all) and brings it back up (post-start
        /// hooks and all), so a restart is a full `eph down` + `eph dev`, not a
        /// bare process bounce. Without `--watch` the stack never restarts.
        #[arg(long = "watch", value_name = "GLOB")]
        watch: Vec<String>,

        /// Bring the stack up and tear it down without running any lifecycle
        /// hooks, matching `eph up --skip-hooks` / `eph down --skip-hooks`.
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

    /// Update eph to the latest GitHub release, replacing the running binary.
    ///
    /// Resolves the latest published release, downloads the archive built for
    /// this platform, verifies it against the release SHA-256 checksums, and
    /// swaps it over the running binary in place (no shell or curl). It installs
    /// the same bits as `scripts/install.sh`, so a self-update and a fresh
    /// install converge.
    Update {
        /// Report whether an update is available without installing it.
        #[arg(long)]
        check: bool,

        /// Reinstall the latest release even when already up to date.
        #[arg(long)]
        force: bool,
    },

    /// Internal: refresh the cached latest-release lookup, then exit.
    ///
    /// Spawned detached by the startup update check (see `eph::update`); not part
    /// of the user-facing command set, so it is hidden from help.
    #[command(name = "__update-check", hide = true)]
    UpdateCheck,
}

/// System subcommands that operate outside the current workspace.
#[derive(Subcommand)]
enum SystemCommand {
    /// Remove resources for deleted or empty workspaces.
    Prune {
        /// Print what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,

        /// Also prune state directories written by eph v0.4.2 and earlier.
        #[arg(long)]
        compatibility_v042: bool,

        /// Remove a stale workspace's resources even if it still has running
        /// containers or a live `run=` process. Without this, a workspace
        /// whose recorded path is gone only because it was moved or renamed
        /// (not truly deleted) is reported and left alone instead of
        /// force-killed.
        #[arg(long)]
        force_live: bool,

        /// Skip the removal confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
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

    // Passively nudge a user on an old release to upgrade, before running the
    // command they asked for. This reads a small on-disk cache and refreshes it
    // in a detached background process, so it never blocks or fails the command
    // itself. Skipped for the updater and the internal refresh worker.
    maybe_nag_about_update(&cli.command);

    match cli.command {
        Commands::Up {
            services,
            roles,
            skip_hooks,
        } => cmd_up(services, roles, skip_hooks)
            .await
            .map(|()| ExitCode::SUCCESS),
        Commands::Down {
            services,
            roles,
            rm,
            skip_hooks,
        } => cmd_down(services, roles, rm, skip_hooks)
            .await
            .map(|()| ExitCode::SUCCESS),
        Commands::Clean { skip_hooks } => cmd_clean(skip_hooks).await.map(|()| ExitCode::SUCCESS),
        Commands::System { command } => match command {
            SystemCommand::Prune {
                dry_run,
                compatibility_v042,
                force_live,
                yes,
            } => cmd_system_prune(dry_run, compatibility_v042, force_live, yes)
                .await
                .map(|()| ExitCode::SUCCESS),
        },
        Commands::Dev {
            service,
            clean,
            watch,
            skip_hooks,
        } => cmd_dev(service, clean, watch, skip_hooks).await,
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
        Commands::Update { check, force } => {
            cmd_update(check, force).await.map(|()| ExitCode::SUCCESS)
        }
        Commands::UpdateCheck => cmd_update_check_worker().await.map(|()| ExitCode::SUCCESS),
    }
}

/// Which direction a `--role` selection resolves in: `Up` pulls in each role's
/// dependencies, `Down` pulls in each role's dependents. See
/// [`resolve_service_selection`].
#[derive(Clone, Copy)]
enum Direction {
    Up,
    Down,
}

/// Turn a command's positional SERVICE names and `--role` values into the set of
/// service names to act on.
///
/// Positional names are validated against the file and taken as-is. Each `--role`
/// expands to its services plus, in the requested direction, the services it
/// depends on (`Up`) or that depend on it (`Down`). The two are unioned, order
/// preserved and duplicates dropped, so `eph up web --role dep` starts `web` and
/// the whole dependency tier. An empty result means "act on everything", matching
/// a bare `eph up` / `eph down`.
fn resolve_service_selection(
    eph: &EphFile,
    services: Vec<String>,
    roles: &[String],
    dir: Direction,
) -> Result<Vec<String>> {
    for name in &services {
        if !eph.services.contains_key(name) {
            anyhow::bail!("unknown service: {}", name);
        }
    }
    let mut names = services;
    if !roles.is_empty() {
        let from_roles = match dir {
            Direction::Up => eph.services_for_roles_up(roles)?,
            Direction::Down => eph.services_for_roles_down(roles)?,
        };
        for name in from_roles {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    Ok(names)
}

async fn cmd_up(services: Vec<String>, roles: Vec<String>, skip_hooks: bool) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let service_filter = resolve_service_selection(&eph, services, &roles, Direction::Up)?;

    let mut manager = ServiceManager::new(workspace).await?;

    let running = manager
        .start_services(&eph, &service_filter, Hooks::from_skip_flag(skip_hooks))
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

async fn cmd_down(
    services: Vec<String>,
    roles: Vec<String>,
    rm: bool,
    skip_hooks: bool,
) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let targets = resolve_service_selection(&eph, services, &roles, Direction::Down)?;

    let mut manager = ServiceManager::new(workspace).await?;

    let action = if rm { "stopped and removed" } else { "stopped" };

    if targets.is_empty() {
        manager.stop_all(&eph, rm, skip_hooks).await?;
        println!("All services {}", action);
    } else {
        // Tear the subset down in reverse start order (dependents before the
        // dependencies they need), persisting the dropped state entries.
        manager
            .stop_selected(&eph, &targets, rm, skip_hooks)
            .await?;
        for name in &targets {
            println!("{} {}", if rm { "Removed" } else { "Stopped" }, name);
        }
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

/// `eph system prune`: report what would be torn down for stale workspaces,
/// then (unless `--dry-run`) confirm and actually tear it down.
///
/// A plain `--dry-run` request is a single pass: list, print, done. A real
/// prune runs the exact same listing pass first (`dry_run: true` under the
/// hood) so the confirmation prompt shows precisely what is about to be
/// removed, including any workspace the liveness guard would skip; only after
/// that plan is shown and confirmed does a second pass, with `dry_run: false`,
/// perform the removal. The two passes can in principle race with something
/// changing on disk or in Docker between them, but `prune` already re-checks
/// each workspace's staleness right before acting on it, the same protection
/// a single dry-run-then-prompt-then-act flow would need anyway.
async fn cmd_system_prune(
    dry_run: bool,
    compatibility_v042: bool,
    force_live: bool,
    yes: bool,
) -> Result<()> {
    let options = PruneOptions {
        dry_run: true,
        compatibility_v042,
        force_live,
    };

    if dry_run {
        let report = eph::prune(options).await?;
        print_prune_report(&report);
        return Ok(());
    }

    let preview = eph::prune(options).await?;
    print_prune_report(&preview);

    match eph::confirmation_outcome(!preview.totals.is_empty(), yes, io::stdin().is_terminal()) {
        eph::ConfirmationOutcome::Proceed => {}
        eph::ConfirmationOutcome::RequireYes => {
            anyhow::bail!(
                "stdin is not a terminal, so system prune cannot prompt for confirmation; pass -y/--yes to remove these resources without asking"
            );
        }
        eph::ConfirmationOutcome::Prompt => {
            print!("\nRemove these resources? [y/N] ");
            io::stdout()
                .flush()
                .context("failed to write the prune confirmation prompt")?;

            let mut answer = String::new();
            io::stdin()
                .read_line(&mut answer)
                .context("failed to read the prune confirmation")?;
            let answer = answer.trim();
            if !answer.eq_ignore_ascii_case("y") && !answer.eq_ignore_ascii_case("yes") {
                println!("Aborted; nothing removed.");
                return Ok(());
            }
        }
    }

    let report = eph::prune(PruneOptions {
        dry_run: false,
        compatibility_v042,
        force_live,
    })
    .await?;
    print_prune_report(&report);
    Ok(())
}

fn print_prune_report(report: &PruneReport) {
    let title = if report.dry_run {
        "System prune dry run:"
    } else {
        "System prune complete:"
    };
    println!("{title}");

    if report.pruned.is_empty() {
        println!("  No stale workspaces found");
    } else {
        for workspace in &report.pruned {
            let path = workspace
                .workspace_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<workspace metadata unavailable>".to_string());
            println!("  {} ({}) - {}", workspace.short_id, workspace.reason, path);
            println!(
                "    containers: {}, volumes: {}, networks: {}, images: {}, run processes: {}, state dirs: {}",
                workspace.counts.containers,
                workspace.counts.volumes,
                workspace.counts.networks,
                workspace.counts.images,
                workspace.counts.processes,
                workspace.counts.state_dirs
            );
        }
    }

    println!();
    println!("Totals:");
    println!("  Containers: {}", report.totals.containers);
    println!("  Volumes: {}", report.totals.volumes);
    println!("  Networks: {}", report.totals.networks);
    println!("  Images: {}", report.totals.images);
    println!("  Verified run= processes: {}", report.totals.processes);
    println!("  State directories: {}", report.totals.state_dirs);

    if !report.skipped.is_empty() {
        println!();
        println!("Skipped:");
        for skipped in &report.skipped {
            let path = skipped
                .workspace_path
                .as_ref()
                .map(|p| format!(" ({})", p.display()))
                .unwrap_or_default();
            println!("  {}{} - {}", skipped.short_id, path, skipped.reason);
        }
    }

    if !report.warnings.is_empty() {
        println!();
        println!("Warnings:");
        for warning in &report.warnings {
            println!("  {warning}");
        }
    }
}

/// Why [`cmd_dev`] stopped blocking in the foreground.
enum DevStop {
    /// A shutdown signal arrived: the preview server stopped us, or Ctrl-C.
    Signal,
    /// The foregrounded app process exited on its own (it crashed or finished),
    /// carrying the wait result so the message can name how it ended.
    AppExited(std::io::Result<std::process::ExitStatus>),
    /// A watched file changed, carrying the workspace-relative path that matched,
    /// so `eph dev` should restart the whole stack. Only reached with `--watch`.
    FileChanged(PathBuf),
}

/// `eph dev`: bring the whole stack up, foreground a `run=` service for a Claude
/// Desktop preview server, and tear it down when stopped.
///
/// The foreground app inherits eph's stdin/stdout/stderr (so it is interactive
/// and its output streams straight through), and eph holds its process handle so
/// it can wait on it directly. Returns a non-zero exit when the app exits on its
/// own, so the preview server sees the dev server went down; a clean stop signal
/// tears the stack down and exits zero.
///
/// With one or more `watch` globs it stays running across restarts: when a
/// matching file changes it tears the whole stack down (pre-stop hooks and all)
/// and brings it back up (post-start hooks and all), then keeps watching. In
/// watch mode an app that exits on its own (a crash) does not end the session:
/// eph reports it and waits for the next change to restart, the way a dev-loop
/// watcher should, since editing is exactly when the app is most likely to
/// crash. Without `--watch` that same exit is reported as a failure and ends
/// `eph dev`, so a preview server sees the dev server went down.
async fn cmd_dev(
    service: Option<String>,
    clean: bool,
    watch: Vec<String>,
    skip_hooks: bool,
) -> Result<ExitCode> {
    let workspace = Workspace::find_from_cwd()?;
    // The watcher matches globs relative to the workspace root, so capture it
    // before `workspace` is moved into the manager.
    let workspace_root = workspace.path.clone();
    let eph = load_eph_file(&workspace)?;

    // Decide which run= service to foreground before touching Docker, so a
    // misconfigured request fails fast with a clear message.
    let foreground = select_foreground_service(&eph, service.as_deref())?;

    let mut manager = ServiceManager::new(workspace).await?;

    let already_running = manager.status().await?;
    // `eph dev` spawns and attaches to the foreground app itself (see
    // `start_foreground`, which never adopts an existing process). It therefore
    // cannot foreground one that is already running: doing so would spawn a second
    // copy and overwrite the original's state entry, orphaning it beyond eph's
    // reach. Fail fast with a clear message instead. A prewarmed dependency tier
    // is fine; only the app being foregrounded is the conflict.
    if already_running.contains_key(foreground.as_str()) {
        anyhow::bail!(
            "the foreground service '{foreground}' is already running; stop it first \
             with `eph down {foreground}` (eph dev starts and attaches to it itself)"
        );
    }
    // Services already running now (a SessionStart hook's prewarmed dependency
    // tier, typically) are adopted and left running on teardown. Everything else,
    // including the foreground just guaranteed not to be running, eph dev brings up
    // and is responsible for tearing back down. Snapshotting here, before the first
    // bring-up, is the whole ownership model: no persisted refcount required.
    // `--clean` overrides this and bulldozes everything, as an explicit full reset.
    let brought_up: Vec<String> = eph
        .services
        .keys()
        .filter(|name| !already_running.contains_key(name.as_str()))
        .cloned()
        .collect();

    // A preview server (Claude Desktop) assigns a host port, passes it as $PORT,
    // then polls it and reveals the app the instant it accepts a connection. We
    // deliberately do NOT let the app bind that port itself: if it did, the
    // preview would go live as soon as the server could answer its health check,
    // which is *before* the post-start seed runs (often ~30s later), leaving the
    // agent staring at an empty app. Instead the app binds its own internal port,
    // and eph opens $PORT only after post-start hooks finish (the gate below), so
    // "the preview is ready" reliably means "seeding is done".
    let gate_port = preview_port();

    // Start watching before the first bring-up so a change made while the stack
    // is coming up is still caught the moment we reach the select loop. A
    // malformed glob or an unwatchable root fails fast, before touching Docker.
    let mut watcher = if watch.is_empty() {
        None
    } else {
        Some(Watch::new(&workspace_root, &watch)?)
    };

    // Restart loop: each pass brings the whole stack up, then blocks until it is
    // stopped, the app exits, or (when watching) a file changes. Without a
    // watcher the loop runs exactly once, matching the original single-shot `eph
    // dev` behavior.
    let mut first = true;
    loop {
        let (mut child, gate) =
            dev_bring_up(&mut manager, &eph, &foreground, gate_port, skip_hooks).await?;

        announce_serving(&manager, &foreground, clean, &watch, first, gate_port).await;
        first = false;

        // Block until a stop signal, the app exiting on its own, or a watched
        // file changing. `child.wait` reaps the process, so its exit is observed
        // reliably (no zombie races). The watch arm is inert without `--watch`.
        let stop = tokio::select! {
            () = wait_for_shutdown() => DevStop::Signal,
            result = child.wait() => DevStop::AppExited(result),
            changed = next_change(&mut watcher), if watcher.is_some() => DevStop::FileChanged(changed),
        };

        // The current app instance is going away in every branch below (torn
        // down, crashed, or about to be restarted), so drop its gate. Closing
        // $PORT lets the preview observe the server go down, and frees the port
        // so the next bring-up can rebind it. Await the aborted task so its
        // listener is released before the loop rebinds $PORT on a restart.
        if let Some(handle) = gate {
            handle.abort();
            let _ = handle.await;
        }

        match stop {
            DevStop::Signal => {
                final_teardown(&mut manager, &eph, clean, &brought_up, skip_hooks).await?;
                // Reap the foreground child we just tore down.
                let _ = child.wait().await;
                return Ok(ExitCode::SUCCESS);
            }
            DevStop::AppExited(result) => {
                // Reap the process that exited on its own before deciding what to do.
                let _ = child.wait().await;
                let how = describe_exit(&result);

                // Without a watcher this is the terminal preview-server contract:
                // leave the stack up for inspection and report failure so the
                // preview server sees the dev server went down.
                let Some(watcher) = watcher.as_mut() else {
                    eprintln!(
                        "dev server '{foreground}' {how}; backing services left up (`eph down` to stop)"
                    );
                    return Ok(ExitCode::FAILURE);
                };

                // Watch mode: a crash should not end the session. Keep the backing
                // services up and wait for the next change (or a stop signal) to
                // restart, so saving a fix brings the app straight back.
                eprintln!();
                eprintln!("dev server '{foreground}' {how}; waiting for a change to restart");
                tokio::select! {
                    () = wait_for_shutdown() => {
                        final_teardown(&mut manager, &eph, clean, &brought_up, skip_hooks).await?;
                        return Ok(ExitCode::SUCCESS);
                    }
                    path = watcher.changed_or_pending() => {
                        restart_banner(&path);
                        // Fall through to the uniform full restart below.
                        manager.stop_selected(&eph, &brought_up, false, false).await?;
                    }
                }
            }
            DevStop::FileChanged(path) => {
                // A restart is a full down + up, including hooks: stop the stack
                // (pre-stop hooks and all), keeping containers and volume data for
                // a fast restart, then loop to bring it back up. Only the services
                // eph dev brought up are bounced; adopted prewarmed dependencies
                // stay up across the restart. A change never drops volumes, even
                // under `--clean`; that teardown is reserved for the final stop.
                restart_banner(&path);
                manager
                    .stop_selected(&eph, &brought_up, false, false)
                    .await?;
                // Reap the foreground child torn down with the stack before the
                // next pass spawns its replacement.
                let _ = child.wait().await;
            }
        }
    }
}

/// Tear the stack down on a final stop.
///
/// With `--clean` this bulldozes the whole workspace (`eph clean`: drop every
/// service and its volume data), the explicit full-reset path. Without it, only
/// the services `eph dev` brought up are stopped (`brought_up`), leaving any it
/// adopted (a session hook's prewarmed dependencies) running for a fast restart
/// or the next command. Shared by the stop-signal path and the watch-mode
/// "crashed, then stopped" path so both honor `--clean` identically.
async fn final_teardown(
    manager: &mut ServiceManager,
    eph: &EphFile,
    clean: bool,
    brought_up: &[String],
    skip_hooks: bool,
) -> Result<()> {
    eprintln!();
    if clean {
        manager.clean(eph, skip_hooks).await?;
        eprintln!("Workspace cleaned");
    } else {
        manager
            .stop_selected(eph, brought_up, false, skip_hooks)
            .await?;
        eprintln!("Services stopped");
    }
    Ok(())
}

/// Describe how the foreground app ended, for the stderr chrome.
fn describe_exit(result: &std::io::Result<std::process::ExitStatus>) -> String {
    match result {
        Ok(status) => format!("exited ({status})"),
        Err(e) => format!("could not be waited on ({e})"),
    }
}

/// Print the "restarting" banner naming the file that triggered it.
fn restart_banner(path: &Path) {
    eprintln!();
    eprintln!("Change detected ({}); restarting dev stack", path.display());
}

/// Bring the whole `eph dev` stack up: pre-start hooks, backing services, the
/// foreground app, every post-start hook, then (once seeding is done) the
/// preview-facing port gate. Returns the live foreground child for the caller to
/// wait on, plus the gate task's handle (`None` when there is no preview `$PORT`
/// or the app already owns it) for the caller to abort on teardown. Factored out
/// of [`cmd_dev`] so the restart loop can rerun the exact same sequence, hooks and
/// gate alike, on every file change.
async fn dev_bring_up(
    manager: &mut ServiceManager,
    eph: &EphFile,
    foreground: &str,
    gate_port: Option<u16>,
    skip_hooks: bool,
) -> Result<(tokio::process::Child, Option<tokio::task::JoinHandle<()>>)> {
    // Two steps so the foreground app inherits eph's stdio. First bring the
    // backing services up with `eph up`'s exact hook interleaving (each
    // service's pre-start runs just before that service is created, so it can
    // reference the services already up); post-start is deferred below so it
    // can also reference the foreground app. Then start the app in the
    // foreground, running its own pre-start immediately before it, again
    // matching `up`. `start_services` with an empty filter would start
    // everything, so only call it when there is at least one backing service.
    let backing: Vec<String> = eph
        .services
        .keys()
        .filter(|name| *name != foreground)
        .cloned()
        .collect();
    let hooks = if skip_hooks {
        Hooks::None
    } else {
        Hooks::PreStartOnly
    };
    if !backing.is_empty() {
        manager.start_services(eph, &backing, hooks).await?;
    }
    if !skip_hooks {
        manager.run_pre_start_for(eph, foreground).await?;
    }
    let (fg, child) = manager.start_foreground(eph, foreground).await?;

    // Everything is healthy now, so run post-start hooks (seeding) for every
    // service, preserving the `eph up` rule that a hook may reference any
    // service's assigned port.
    if !skip_hooks {
        manager.run_all_post_start(eph).await?;
    }

    // Seeding is done, so open the preview-facing gate. Binding $PORT here, and
    // not one step earlier, is the whole point: the preview server watches this
    // port, so it only now sees the app as ready. The app is on its own internal
    // port; the gate forwards each connection to it. A bind failure (for example
    // $PORT already taken) surfaces here, before we advertise the URL.
    let gate = match gate_port {
        // If the app happened to land on the preview's exact port, there is
        // nothing to forward and nothing to hold closed; serve it directly.
        Some(gp) if fg.port() == Some(gp) => None,
        Some(gp) => {
            let app_port = fg.port().context(
                "eph dev received a preview $PORT but the foreground service exposes no port",
            )?;
            let listener = bind_preview_gate(gp)?;
            Some(tokio::spawn(serve_port_gate(listener, app_port)))
        }
        None => None,
    };

    Ok((child, gate))
}

/// Print the "Serving ..." chrome to stderr (the app keeps stdout to itself).
///
/// The serving URL is reprinted on every restart so a watch-driven bounce shows
/// where the fresh server is listening. The one-time hints (how to stop, and what
/// is being watched) print only on the `first` pass to keep restart output terse.
async fn announce_serving(
    manager: &ServiceManager,
    foreground: &str,
    clean: bool,
    watch: &[String],
    first: bool,
    gate_port: Option<u16>,
) {
    // The preview connects to the gate port when there is one, so advertise that;
    // otherwise the app owns its port directly, read back from the running set
    // (rather than threaded out of bring-up) so a restart on a new auto port is
    // reported accurately.
    let port = match gate_port {
        Some(gp) => Some(gp),
        None => manager
            .status()
            .await
            .ok()
            .and_then(|running| running.get(foreground).and_then(RunningService::port)),
    };
    if let Some(port) = port {
        eprintln!();
        eprintln!("Serving '{foreground}' on http://localhost:{port}");
    }
    if first {
        let teardown = if clean { "eph clean" } else { "eph down" };
        eprintln!("Press Ctrl-C to stop ({teardown} on exit)");
        if !watch.is_empty() {
            eprintln!("Watching for changes: {}", watch.join(", "));
        }
    }
}

/// Await the next matching file change, or never resolve when not watching.
///
/// The `tokio::select!` precondition already gates the watch arm on
/// `watcher.is_some()`, but this also parks forever if the watcher has shut down
/// (its channel closed), so a dead watcher can never spuriously restart the
/// stack. It only resolves with a real change.
async fn next_change(watcher: &mut Option<Watch>) -> PathBuf {
    match watcher {
        Some(watch) => watch.changed_or_pending().await,
        None => std::future::pending().await,
    }
}

/// Read `$PORT` as a preview-server-assigned host port, if set and valid.
///
/// Claude Desktop's preview server picks a free port and passes it as `PORT`
/// (its `autoPort` behavior); an empty or non-numeric value is ignored so a
/// stray environment variable cannot wedge `eph dev` startup.
fn preview_port() -> Option<u16> {
    std::env::var("PORT").ok()?.trim().parse().ok()
}

/// Bind the preview-facing gate on loopback `port` with `SO_REUSEADDR`.
///
/// `eph dev` reopens this port on every bring-up, so a `--watch` restart rebinds
/// the same fixed `$PORT` moments after the previous gate was torn down. The
/// abort-and-await in the restart loop releases the old gate's *listening* socket,
/// but the in-flight connections it forwarded run on detached tasks whose sockets
/// can outlive it in `TIME_WAIT`. Without `SO_REUSEADDR` a fresh bind then races
/// them: on Windows (mio deliberately omits the option there) and some BSDs that
/// bind fails with `EADDRINUSE`, which would propagate out and kill the dev
/// session mid-edit. tokio's default `bind` does not set the option, so build the
/// listener by hand. The port is loopback-only, so the option's Windows
/// address-hijacking caveat does not apply.
fn bind_preview_gate(port: u16) -> Result<tokio::net::TcpListener> {
    use socket2::{Domain, Socket, Type};

    let addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, port).into();
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None)
        .context("failed to create the preview gate socket")?;
    socket
        .set_reuse_address(true)
        .context("failed to set SO_REUSEADDR on the preview gate socket")?;
    socket
        .bind(&addr.into())
        .with_context(|| format!("failed to bind preview port {port} for `eph dev`"))?;
    socket
        .listen(1024)
        .with_context(|| format!("failed to listen on preview port {port} for `eph dev`"))?;
    // tokio requires the std listener to be non-blocking before it adopts it.
    socket
        .set_nonblocking(true)
        .context("failed to make the preview gate socket non-blocking")?;
    tokio::net::TcpListener::from_std(socket.into())
        .context("failed to register the preview gate listener with tokio")
}

/// Forward every connection on the preview-facing gate `listener` to the
/// foreground app at `127.0.0.1:app_port`, copying bytes in both directions until
/// either side closes.
///
/// `eph dev` binds the gate only after post-start hooks finish, so a Claude
/// Desktop preview server (which polls the gate port and reveals the app the
/// instant it connects) does not see the app as ready until seeding is done.
/// Keeping the app on its own internal port and forwarding is what lets eph hold
/// that port closed without the app needing to know anything about it. The
/// forwarding is a plain byte splice, so it is transparent to HTTP keep-alive,
/// Server-Sent Events, and websockets alike.
///
/// Runs until aborted (on teardown). A connection that cannot reach the app is
/// dropped on its own, and a transient accept error is skipped, so one bad
/// connection can never take the whole gate down.
async fn serve_port_gate(listener: tokio::net::TcpListener, app_port: u16) {
    loop {
        let Ok((mut inbound, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let Ok(mut outbound) = tokio::net::TcpStream::connect(("127.0.0.1", app_port)).await
            else {
                return;
            };
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
        });
    }
}

/// Choose the `run=` service `eph dev` foregrounds.
///
/// With an explicit `requested` name, that service must exist and be a `run=`
/// service. Otherwise the sole `run=` service is used; zero or several are an
/// error whose message tells the caller how to proceed.
fn select_foreground_service(eph: &EphFile, requested: Option<&str>) -> Result<String> {
    if let Some(name) = requested {
        let service = eph
            .services
            .get(name)
            .with_context(|| format!("unknown service: {name}"))?;
        if !matches!(service.source, ServiceSource::Command(_)) {
            anyhow::bail!(
                "service '{name}' is not a run= service, so `eph dev` cannot foreground it \
                 (it runs a container, not a host process)"
            );
        }
        return Ok(name.to_string());
    }

    let run_services: Vec<&String> = eph
        .services
        .iter()
        .filter(|(_, svc)| matches!(svc.source, ServiceSource::Command(_)))
        .map(|(name, _)| name)
        .collect();

    match run_services.as_slice() {
        [] => anyhow::bail!(
            "`eph dev` foregrounds a run= service, but this .eph defines none \
             (add a [service] with run=)"
        ),
        [only] => Ok((*only).to_string()),
        many => {
            let names = many
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "this .eph defines several run= services ({names}); \
                 name the one to foreground: `eph dev <service>`"
            )
        }
    }
}

/// Block until a shutdown signal arrives.
///
/// A Claude Desktop preview server stops the dev command by signaling it; this
/// catches the platform's stop signals so `eph dev` can tear the stack down
/// before exiting. On Unix that is SIGINT (Ctrl-C) and SIGTERM; on Windows it is
/// Ctrl-C, console close, and system shutdown. A hard kill (SIGKILL /
/// `TerminateProcess`) cannot be caught, so teardown is skipped and the stack is
/// left up, recoverable with `eph down`.
#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    // The handlers only fail to install on an unsupported platform or fd
    // exhaustion, both fatal for a foreground server, so surface them as panics.
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = interrupt.recv() => {}
    }
}

#[cfg(windows)]
async fn wait_for_shutdown() {
    use tokio::signal::windows::{ctrl_c, ctrl_close, ctrl_shutdown};
    let mut interrupt = ctrl_c().expect("install Ctrl-C handler");
    let mut close = ctrl_close().expect("install console-close handler");
    let mut shutdown = ctrl_shutdown().expect("install shutdown handler");
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = close.recv() => {}
        _ = shutdown.recv() => {}
    }
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
        match &svc.role {
            Some(role) => println!("  {} [{}] ({})", name, role, source),
            None => println!("  {} ({})", name, source),
        }
    }

    // In roles mode, show the tiers and the resulting bring-up order so the
    // dependency-vs-app split (and what `--role` will select) is visible at a
    // glance without running Docker.
    if eph.roles_order.is_some() {
        let order: Vec<&str> = eph.start_order().iter().map(|s| s.as_str()).collect();
        println!();
        println!("Bring-up order: {}", order.join(", "));
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

/// Emit the startup out-of-date nag for a normal command.
///
/// Skips the updater itself (it reports status directly) and the internal refresh
/// worker (which must not recurse into another check). Everything else defers to
/// [`eph::update::warn_if_outdated`], which applies the remaining gates (release
/// build, interactive stderr, opt-out env) and spawns the background refresh.
fn maybe_nag_about_update(command: &Commands) {
    if matches!(command, Commands::Update { .. } | Commands::UpdateCheck) {
        return;
    }
    eph::update::warn_if_outdated(env!("EPH_VERSION"));
}

/// The detached background worker (`eph __update-check`) spawned by the startup
/// check: refresh the cached latest release, silently, then exit. Any error is
/// swallowed so a failed refresh never surfaces; the cache is retried later.
async fn cmd_update_check_worker() -> Result<()> {
    tokio::task::spawn_blocking(eph::update::run_check_worker)
        .await
        .context("the update-check worker panicked")
}

/// `eph update`: resolve the latest release and swap the running binary for it.
///
/// The network and filesystem work (HTTPS download, archive extraction, the
/// in-place binary swap) is synchronous and blocking, so it runs on a blocking
/// thread rather than stalling the async runtime the rest of eph shares.
async fn cmd_update(check: bool, force: bool) -> Result<()> {
    tokio::task::spawn_blocking(move || run_update(check, force))
        .await
        .context("the update task panicked")?
}

/// The blocking body of [`cmd_update`], factored out so it runs on a blocking
/// thread. Resolves the latest release, then either reports status (`--check`) or
/// downloads, verifies, and installs it.
fn run_update(check: bool, force: bool) -> Result<()> {
    use eph::update::{self, Status};

    let current = env!("EPH_VERSION");
    let updater = update::Updater::new();
    let latest = updater
        .latest_tag()
        .context("resolve the latest eph release")?;
    let status = update::status(current, &latest);

    if check {
        print_update_status(current, &latest, status);
        return Ok(());
    }
    if status == Status::UpToDate && !force {
        println!("eph {current} is already the latest release.");
        return Ok(());
    }

    match status {
        // A development build has no meaningful ordering against the release, so
        // be explicit that this installs the latest published release over it.
        Status::Development => println!(
            "Installing the latest release {latest} (replacing development build {current})."
        ),
        _ => println!("Updating eph from {current} to {latest}."),
    }

    // Stage the download in a temp file, then let self-replace swap it over the
    // running binary. The staged file is copied into place inside `fetch` +
    // `replace_running_exe`; dropping it afterward removes the leftover.
    let staged = tempfile::Builder::new()
        .prefix("eph-update-")
        .tempfile()
        .context("create a staging file for the update")?;
    updater.fetch(&latest, staged.path())?;
    update::replace_running_exe(staged.path())?;
    drop(staged);

    println!("eph updated to {latest}.");
    Ok(())
}

/// Print the result of `eph update --check`.
fn print_update_status(current: &str, latest: &str, status: eph::update::Status) {
    use eph::update::Status;
    match status {
        Status::Development => {
            println!("eph is a development build ({current}); the latest release is {latest}.");
            println!("Run `eph update` to install it.");
        }
        Status::UpToDate => {
            println!("eph {current} is up to date (latest release {latest}).");
        }
        Status::UpdateAvailable => {
            println!("update available: {latest} (current {current}).");
            println!("Run `eph update` to install it.");
        }
    }
}

fn load_eph_file(workspace: &Workspace) -> Result<EphFile> {
    let path = workspace.eph_file_path();
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parser::parse(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_eph(input: &str) -> EphFile {
        parser::parse(input).expect("test .eph should parse")
    }

    #[test]
    fn dev_foreground_defaults_to_the_sole_run_service() {
        let eph = parse_eph(
            "[postgres]\nimage=postgres:16\nport=5432\n[web]\nrun=npm run dev\nport=auto\n",
        );
        assert_eq!(select_foreground_service(&eph, None).unwrap(), "web");
    }

    #[test]
    fn dev_foreground_picks_the_named_run_service() {
        let eph = parse_eph("[web]\nrun=npm run dev\nport=auto\n[worker]\nrun=npm run worker\n");
        assert_eq!(
            select_foreground_service(&eph, Some("worker")).unwrap(),
            "worker"
        );
    }

    #[test]
    fn dev_foreground_rejects_a_container_service() {
        let eph = parse_eph("[postgres]\nimage=postgres:16\nport=5432\n[web]\nrun=npm run dev\n");
        let err = select_foreground_service(&eph, Some("postgres"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a run= service"), "got: {err}");
    }

    #[test]
    fn dev_foreground_rejects_an_unknown_service() {
        let eph = parse_eph("[web]\nrun=npm run dev\n");
        let err = select_foreground_service(&eph, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown service"), "got: {err}");
    }

    #[test]
    fn dev_foreground_requires_a_run_service() {
        let eph = parse_eph("[postgres]\nimage=postgres:16\nport=5432\n");
        let err = select_foreground_service(&eph, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("defines none"), "got: {err}");
    }

    #[test]
    fn dev_foreground_is_ambiguous_with_several_run_services() {
        let eph = parse_eph("[web]\nrun=npm run dev\n[worker]\nrun=npm run worker\n");
        let err = select_foreground_service(&eph, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("several run= services"), "got: {err}");
        assert!(err.contains("web") && err.contains("worker"), "got: {err}");
    }

    /// The gate is a transparent forwarder: a client that reaches the gate port is
    /// really talking to the app, in both directions. Uses a loopback echo server
    /// as the stand-in app and drives a full request/response through the gate.
    #[tokio::test]
    async fn port_gate_forwards_bytes_both_ways() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        // Stand-in "app": an echo server on a free loopback port.
        let app = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let app_port = app.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = app.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });

        // Gate on its own free port, forwarding to the echo server.
        let gate = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let gate_port = gate.local_addr().unwrap().port();
        let handle = tokio::spawn(serve_port_gate(gate, app_port));

        // Writing to the gate and reading back should round-trip through the app.
        let mut conn = TcpStream::connect(("127.0.0.1", gate_port)).await.unwrap();
        conn.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        conn.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");

        handle.abort();
    }

    /// A gate whose app never came up (or has gone away) must not wedge or panic:
    /// it accepts the client and then closes it when the upstream connect fails.
    #[tokio::test]
    async fn port_gate_drops_client_when_app_is_unreachable() {
        use tokio::io::AsyncReadExt;
        use tokio::net::{TcpListener, TcpStream};

        // Reserve a port for a "dead app" and immediately free it, so connects to
        // it are refused.
        let dead = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);

        let gate = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let gate_port = gate.local_addr().unwrap().port();
        let handle = tokio::spawn(serve_port_gate(gate, dead_port));

        // The gate accepts, fails to reach the app, and closes the client, so the
        // read returns EOF (0 bytes) rather than hanging.
        let mut conn = TcpStream::connect(("127.0.0.1", gate_port)).await.unwrap();
        let mut buf = [0u8; 8];
        let n = conn.read(&mut buf).await.unwrap();
        assert_eq!(
            n, 0,
            "expected the gate to close a client it cannot forward"
        );

        handle.abort();
    }

    /// Regression: a `--watch` restart must be able to rebind the same fixed
    /// `$PORT` immediately after the previous gate is torn down, even while an
    /// old forwarded connection still lingers on that port. Without SO_REUSEADDR
    /// (see `bind_preview_gate`) that rebind fails on Windows with `EADDRINUSE`
    /// and kills the dev session mid-edit. Mirrors the loop's abort-then-rebind.
    #[tokio::test]
    async fn preview_gate_rebinds_the_same_port_after_teardown() {
        use tokio::net::{TcpListener, TcpStream};

        // A stand-in app whose port the gate forwards to; it need not accept, the
        // OS completes the connect into its backlog, which is enough to keep the
        // forwarded connection (and its socket on the gate port) alive.
        let app = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let app_port = app.local_addr().unwrap().port();

        // Reserve a free port to act as the fixed preview `$PORT`, then free it so
        // the gate can take it.
        let probe = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        // First gate generation on the fixed port, with a live client so a
        // connection socket is bound to that port when we tear the gate down.
        let listener = bind_preview_gate(port).expect("first gate bind");
        let handle = tokio::spawn(serve_port_gate(listener, app_port));
        let _client = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("client connects to the gate");

        // Tear the gate down exactly as cmd_dev's restart path does.
        handle.abort();
        let _ = handle.await;

        // The next bring-up must rebind the same port right away. This is the
        // assertion that would fail on Windows without SO_REUSEADDR.
        let rebound = bind_preview_gate(port)
            .expect("gate must rebind the same $PORT immediately after teardown");
        drop(rebound);
    }
}
