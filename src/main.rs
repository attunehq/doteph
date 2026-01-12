mod parser;
mod service;
mod workspace;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use parser::EphFile;
use service::ServiceManager;
use std::collections::HashMap;
use tracing_subscriber::EnvFilter;
use workspace::Workspace;

#[derive(Parser)]
#[command(name = "eph")]
#[command(about = "Ephemeral services per workspace - dotenv for services")]
#[command(version)]
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
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    match cli.command {
        Commands::Up { services } => cmd_up(services).await,
        Commands::Down { services } => cmd_down(services).await,
        Commands::Status => cmd_status().await,
        Commands::Env { format } => cmd_env(&format).await,
        Commands::Check => cmd_check().await,
        Commands::Info => cmd_info().await,
    }
}

async fn cmd_up(service_filter: Vec<String>) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let mut manager = ServiceManager::new(workspace.clone()).await?;

    let running = if service_filter.is_empty() {
        manager.start_all(&eph).await?
    } else {
        let mut running = HashMap::new();
        for name in &service_filter {
            let service = eph.services.get(name)
                .with_context(|| format!("Unknown service: {}", name))?;
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

async fn cmd_down(service_filter: Vec<String>) -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let mut manager = ServiceManager::new(workspace).await?;

    if service_filter.is_empty() {
        manager.stop_all(&eph).await?;
        println!("All services stopped");
    } else {
        for name in &service_filter {
            let service = eph.services.get(name)
                .with_context(|| format!("Unknown service: {}", name))?;
            manager.stop_service(name, service).await?;
            println!("Stopped {}", name);
        }
    }

    Ok(())
}

async fn cmd_status() -> Result<()> {
    let workspace = Workspace::find_from_cwd()?;
    let eph = load_eph_file(&workspace)?;

    let manager = ServiceManager::new(workspace.clone()).await?;
    let running = manager.status().await?;

    println!("Workspace: {}", workspace.path.display());
    println!("ID: {}", workspace.short_id);
    println!();

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

        let stopped: Vec<_> = eph.services.keys()
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

    let manager = ServiceManager::new(workspace.clone()).await?;
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

    // Output in requested format
    match format {
        "export" => {
            for (name, value) in &env_vars {
                println!("export {}=\"{}\"", name, escape_bash(value));
            }
        }
        "fish" => {
            for (name, value) in &env_vars {
                println!("set -gx {} \"{}\"", name, escape_fish(value));
            }
        }
        "json" => {
            let map: HashMap<_, _> = env_vars.into_iter().collect();
            println!("{}", serde_json::to_string_pretty(&map)?);
        }
        _ => {
            anyhow::bail!("Unknown format: {}. Use: export, fish, json", format);
        }
    }

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
            parser::ServiceSource::Image(img) => format!("image: {}", img),
            parser::ServiceSource::Dockerfile(path) => format!("dockerfile: {}", path),
            parser::ServiceSource::Compose(path) => format!("compose: {}", path),
            parser::ServiceSource::Command(cmd) => format!("command: {}", cmd),
            parser::ServiceSource::None => "none".to_string(),
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
        .with_context(|| format!("Failed to read {}", path.display()))?;
    parser::parse(&contents)
        .with_context(|| format!("Failed to parse {}", path.display()))
}

fn escape_bash(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

fn escape_fish(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
}
