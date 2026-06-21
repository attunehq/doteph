//! Parser for .eph files
//!
//! The .eph format extends .env syntax with INI-style sections for services.
//!
//! # Example
//! ```text
//! # Simple environment variables (like .env)
//! APP_NAME=myapp
//! DEBUG=true
//!
//! # Service definitions use INI-style sections
//! [postgres]
//! image=postgres:16
//! port=5432
//! env.POSTGRES_USER=dev
//! env.POSTGRES_PASSWORD=dev
//! env.POSTGRES_DB=app
//! post-start=cargo sqlx migrate run
//!
//! # Environment variables can interpolate service properties
//! DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/app
//! ```

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::warn;

// ============================================================================
// AST Types
// ============================================================================

/// A parsed .eph file
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EphFile {
    /// Top-level environment variables
    pub env_vars: Vec<EnvVar>,
    /// Service definitions (keyed by service name)
    pub services: HashMap<String, Service>,
}

/// An environment variable definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// A service definition
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Service {
    /// Service name (matches section header)
    pub name: String,
    /// How to start this service
    pub source: ServiceSource,
    /// Port mappings (container ports that will be mapped to random host ports)
    pub ports: Vec<PortMapping>,
    /// Environment variables to pass to the container
    pub env: HashMap<String, String>,
    /// Volume mounts (host:container format)
    pub volumes: Vec<String>,
    /// Commands to run after service is ready
    pub post_start: Vec<String>,
    /// Commands to run before stopping the service
    pub pre_stop: Vec<String>,
    /// Health check command
    pub healthcheck: Option<String>,
    /// Timeout in seconds to wait for service to be ready
    pub ready_timeout_secs: Option<u64>,
    /// Build context for Dockerfile builds
    pub build_context: Option<String>,
    /// Command override (replaces the default CMD in the image)
    pub command_override: Option<String>,
}

/// How a service is started
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ServiceSource {
    /// No source specified yet
    #[default]
    None,
    /// Docker image to pull and run
    Image(String),
    /// Dockerfile to build
    Dockerfile(String),
    /// Docker compose file (service name optional)
    Compose(String),
    /// Shell command to run (for non-Docker services)
    Command(String),
}

/// A port mapping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMapping {
    /// Optional name for this port (e.g., "api", "admin")
    pub name: Option<String>,
    /// Container port to expose
    pub container_port: u16,
}

// ============================================================================
// Parser
// ============================================================================

/// Parse an .eph file from a string
pub fn parse(input: &str) -> Result<EphFile> {
    let mut env_vars: Vec<EnvVar> = Vec::new();
    let mut services: HashMap<String, Service> = HashMap::new();
    let mut current_service: Option<String> = None;

    for (line_num, line) in input.lines().enumerate() {
        let line_num = line_num + 1; // 1-indexed
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Check for section header [service_name]
        if line.starts_with('[') && line.ends_with(']') {
            let name = &line[1..line.len() - 1];
            if name.is_empty() {
                bail!("Empty section name at line {}", line_num);
            }
            current_service = Some(name.to_string());
            services.entry(name.to_string()).or_insert_with(|| Service {
                name: name.to_string(),
                ..Default::default()
            });
            continue;
        }

        // Parse key=value
        let Some((key, value)) = line.split_once('=') else {
            bail!("Invalid syntax at line {}: expected KEY=VALUE", line_num);
        };

        let key = key.trim();
        let value = value.trim();

        // Remove optional quotes from value
        let value = strip_quotes(value);

        if let Some(ref service_name) = current_service {
            // We're inside a service section - try to parse as service property
            let service = services.get_mut(service_name).unwrap();
            match parse_service_property(service, key, value, line_num) {
                Ok(()) => continue,
                Err(_) if is_env_var_name(key) => {
                    // Unknown property, but the key looks like a SCREAMING_SNAKE_CASE
                    // env var name. We intentionally end the current section here and
                    // reclassify this key as a top-level environment variable. This
                    // supports files that list service sections first and trailing
                    // env vars without a blank line, but it also silently swallows
                    // typos in service property names, so emit a warning to make the
                    // behavior discoverable.
                    warn!(
                        "Key '{}' inside section [{}] at line {} is not a known service \
                         property; it looks like an environment variable, so the section \
                         was ended and the key was treated as a top-level variable. If you \
                         meant a service property, check for a typo.",
                        key, service_name, line_num
                    );
                    current_service = None;
                    env_vars.push(EnvVar {
                        name: key.to_string(),
                        value: value.to_string(),
                    });
                }
                Err(e) => return Err(e),
            }
        } else {
            // Top-level environment variable
            env_vars.push(EnvVar {
                name: key.to_string(),
                value: value.to_string(),
            });
        }
    }

    Ok(EphFile { env_vars, services })
}

/// Check if a key looks like an environment variable name (SCREAMING_SNAKE_CASE)
fn is_env_var_name(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn strip_quotes(s: &str) -> &str {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn parse_service_property(
    service: &mut Service,
    key: &str,
    value: &str,
    line_num: usize,
) -> Result<()> {
    match key {
        "image" => service.source = ServiceSource::Image(value.to_string()),
        "dockerfile" => service.source = ServiceSource::Dockerfile(value.to_string()),
        "compose" => service.source = ServiceSource::Compose(value.to_string()),
        // Shell command to run (non-Docker)
        "run" => service.source = ServiceSource::Command(value.to_string()),
        // Container command override (for use with image/dockerfile)
        "command" => service.command_override = Some(value.to_string()),
        "port" => {
            let port: u16 = value
                .parse()
                .with_context(|| format!("Invalid port number at line {}", line_num))?;
            service.ports.push(PortMapping {
                name: None,
                container_port: port,
            });
        }
        "volume" => {
            service.volumes.push(value.to_string());
        }
        "post-start" => {
            service.post_start.push(value.to_string());
        }
        "pre-stop" => {
            service.pre_stop.push(value.to_string());
        }
        "healthcheck" => {
            service.healthcheck = Some(value.to_string());
        }
        "ready-timeout" => {
            let secs: u64 = value
                .parse()
                .with_context(|| format!("Invalid timeout at line {}", line_num))?;
            service.ready_timeout_secs = Some(secs);
        }
        key if key.starts_with("port.") => {
            let port_name = &key[5..];
            let port: u16 = value
                .parse()
                .with_context(|| format!("Invalid port number at line {}", line_num))?;
            service.ports.push(PortMapping {
                name: Some(port_name.to_string()),
                container_port: port,
            });
        }
        key if key.starts_with("env.") => {
            let env_name = &key[4..];
            service.env.insert(env_name.to_string(), value.to_string());
        }
        // For compose-based services, expose maps service ports
        key if key.starts_with("expose.") => {
            let port_name = &key[7..];
            let port: u16 = value
                .parse()
                .with_context(|| format!("Invalid port number at line {}", line_num))?;
            service.ports.push(PortMapping {
                name: Some(port_name.to_string()),
                container_port: port,
            });
        }
        // Build context for Dockerfiles
        "context" => {
            service.build_context = Some(value.to_string());
        }
        _ => {
            bail!("Unknown service property '{}' at line {}", key, line_num);
        }
    }
    Ok(())
}

// ============================================================================
// Interpolation
// ============================================================================

/// Replace interpolations in a string using a resolver function
pub fn resolve_interpolations<F>(input: &str, resolver: F) -> String
where
    F: Fn(&str, &str) -> Option<String>,
{
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut content = String::new();

            while let Some(&c) = chars.peek() {
                if c == '}' {
                    chars.next();
                    break;
                }
                content.push(c);
                chars.next();
            }

            if let Some((service, property)) = content.split_once('.') {
                if let Some(value) = resolver(service, property) {
                    result.push_str(&value);
                } else {
                    // Keep original if not resolved
                    result.push_str(&format!("${{{}}}", content));
                }
            } else {
                result.push_str(&format!("${{{}}}", content));
            }
        } else {
            result.push(c);
        }
    }

    result
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_env() {
        let input = r#"
APP_NAME=myapp
DEBUG=true
"#;
        let result = parse(input).unwrap();
        assert_eq!(result.env_vars.len(), 2);
        assert_eq!(result.env_vars[0].name, "APP_NAME");
        assert_eq!(result.env_vars[0].value, "myapp");
    }

    #[test]
    fn test_parse_service() {
        let input = r#"
[postgres]
image=postgres:16
port=5432
env.POSTGRES_USER=dev
"#;
        let result = parse(input).unwrap();
        assert_eq!(result.services.len(), 1);
        let pg = result.services.get("postgres").unwrap();
        assert!(matches!(&pg.source, ServiceSource::Image(img) if img == "postgres:16"));
        assert_eq!(pg.ports[0].container_port, 5432);
        assert_eq!(pg.env.get("POSTGRES_USER"), Some(&"dev".to_string()));
    }

    #[test]
    fn test_parse_interpolation() {
        let input = r#"
[postgres]
image=postgres:16
port=5432

DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/app
"#;
        let result = parse(input).unwrap();
        assert_eq!(
            result.env_vars[0].value,
            "postgres://dev:dev@localhost:${postgres.port}/app"
        );
    }

    #[test]
    fn test_parse_post_start() {
        let input = r#"
[postgres]
image=postgres:16
port=5432
post-start=cargo sqlx migrate run
post-start=cargo sqlx fixtures load
"#;
        let result = parse(input).unwrap();
        let pg = result.services.get("postgres").unwrap();
        assert_eq!(pg.post_start.len(), 2);
    }

    #[test]
    fn test_env_looking_key_in_section_ends_section() {
        // A SCREAMING_SNAKE_CASE key that is not a known service property is
        // intentionally and deterministically reclassified as a top-level env
        // var, ending the current section. (A tracing::warn! is emitted to make
        // this discoverable, but behavior is unchanged.)
        let input = r#"
[postgres]
image=postgres:16
port=5432
DATABASE_URL=postgres://localhost/app
LOG_LEVEL=debug
"#;
        let result = parse(input).unwrap();

        // The service only captured the known properties before the env-looking key.
        let pg = result.services.get("postgres").unwrap();
        assert!(matches!(&pg.source, ServiceSource::Image(img) if img == "postgres:16"));
        assert_eq!(pg.ports.len(), 1);
        assert!(!pg.env.contains_key("DATABASE_URL"));

        // The env-looking keys ended the section and became top-level vars.
        let names: Vec<&str> = result.env_vars.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["DATABASE_URL", "LOG_LEVEL"]);
        assert_eq!(result.env_vars[0].value, "postgres://localhost/app");
    }

    #[test]
    fn test_unknown_non_env_key_in_section_errors() {
        // A non-env-looking unknown property is still a hard error (not silently
        // reclassified), so genuine typos in lowercase keys are caught.
        let input = r#"
[postgres]
image=postgres:16
prot=5432
"#;
        assert!(parse(input).is_err());
    }

    #[test]
    fn test_resolve_interpolations() {
        let input = "postgres://localhost:${postgres.port}/db";
        let result = resolve_interpolations(input, |service, property| {
            if service == "postgres" && property == "port" {
                Some("5432".to_string())
            } else {
                None
            }
        });
        assert_eq!(result, "postgres://localhost:5432/db");
    }
}
