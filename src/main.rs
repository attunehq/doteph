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
use eph::{ServiceManager, Workspace, skills};
use std::collections::HashMap;
use std::io::{self, Write};
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
    },

    /// Stop all services
    Down {
        /// Specific services to stop (defaults to all)
        #[arg(value_name = "SERVICE")]
        services: Vec<String>,

        /// Remove containers after stopping them (instead of just stopping)
        #[arg(short = 'r', long = "rm")]
        rm: bool,
    },

    /// Stop and remove all services, named volumes, and persisted state
    Clean,

    /// Show status of services
    Status,

    /// Print environment variables for shell eval
    /// Usage: eval "$(eph env)"
    Env {
        /// Output format: export (default), fish, json
        #[arg(short, long, default_value = "export")]
        format: String,
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
        Commands::Up { services } => cmd_up(services).await.map(|()| ExitCode::SUCCESS),
        Commands::Down { services, rm } => cmd_down(services, rm).await.map(|()| ExitCode::SUCCESS),
        Commands::Clean => cmd_clean().await.map(|()| ExitCode::SUCCESS),
        Commands::Status => cmd_status().await.map(|()| ExitCode::SUCCESS),
        Commands::Env { format } => cmd_env(&format).await.map(|()| ExitCode::SUCCESS),
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

async fn cmd_up(service_filter: Vec<String>) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let mut manager = ServiceManager::new(workspace).await?;

    let running = if service_filter.is_empty() {
        manager.start_all(&eph).await?
    } else {
        let mut running = HashMap::new();
        for name in &service_filter {
            let service = eph
                .services
                .get(name)
                .with_context(|| format!("unknown service: {}", name))?;
            let result = manager.start_service(name, service).await?;
            running.insert(name.clone(), result);
        }
        // Save state after starting individual services
        manager.save_state().await?;
        running
    };

    // Print summary
    println!();
    println!("Services started:");
    for (name, svc) in &running {
        if let Some(port) = svc.port() {
            println!("  {} -> localhost:{}", name, port);
        } else {
            println!("  {} (no ports)", name);
        }
    }

    // Print environment hint
    println!();
    println!("Run `eval \"$(eph env)\"` to set environment variables");

    Ok(())
}

async fn cmd_down(service_filter: Vec<String>, rm: bool) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let mut manager = ServiceManager::new(workspace).await?;

    let action = if rm { "stopped and removed" } else { "stopped" };

    if service_filter.is_empty() {
        manager.stop_all(&eph, rm).await?;
        println!("All services {}", action);
    } else {
        for name in &service_filter {
            let service = eph
                .services
                .get(name)
                .with_context(|| format!("unknown service: {}", name))?;
            manager.stop_service(name, service, rm).await?;
            println!("{} {}", if rm { "Removed" } else { "Stopped" }, name);
        }
    }

    Ok(())
}

async fn cmd_clean() -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let mut manager = ServiceManager::new(workspace).await?;
    let summary = manager.clean(&eph).await?;

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
        for (name, svc) in &running {
            if let Some(port) = svc.port() {
                println!("  {} -> localhost:{}", name, port);
            } else {
                println!("  {} (no ports)", name);
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

async fn cmd_env(format: &str) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let manager = ServiceManager::new(workspace).await?;
    let running = manager.status().await?;

    // Build resolver for interpolation
    let resolver = |service: &str, property: &str| -> Option<String> {
        let svc = running.get(service)?;
        match property {
            "host" => Some(svc.host().to_string()),
            "port" => svc.port().map(|p| p.to_string()),
            prop if prop.starts_with("port.") => {
                let name = &prop[5..];
                svc.named_port(name).map(|p| p.to_string())
            }
            _ => None,
        }
    };

    // Collect resolved environment variables
    let mut env_vars: Vec<(String, String)> = Vec::new();

    for var in &eph.env_vars {
        let resolved = parser::resolve_interpolations(&var.value, resolver);
        env_vars.push((var.name.clone(), resolved));
    }

    // Render in the requested format
    print!("{}", eph::render(&env_vars, format)?);

    Ok(())
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
