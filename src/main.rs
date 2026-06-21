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
use eph::{ServiceManager, Workspace};
use std::collections::HashMap;
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
}

#[tokio::main]
async fn main() -> Result<()> {
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
        Commands::Up { services } => cmd_up(services).await,
        Commands::Down { services, rm } => cmd_down(services, rm).await,
        Commands::Clean => cmd_clean().await,
        Commands::Status => cmd_status().await,
        Commands::Env { format } => cmd_env(&format).await,
        Commands::Check => cmd_check().await,
        Commands::Info => cmd_info().await,
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

fn load_eph_file(workspace: &Workspace) -> Result<EphFile> {
    let path = workspace.eph_file_path();
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parser::parse(&contents).with_context(|| format!("failed to parse {}", path.display()))
}
