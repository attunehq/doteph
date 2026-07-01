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
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::warn;

// ============================================================================
// AST Types
// ============================================================================

/// A parsed `.eph` file.
///
/// Produced by [`parse`]. Holds the top-level [`EnvVar`]s and the named
/// [`Service`] definitions extracted from the file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EphFile {
    /// Top-level environment variables, in declaration order.
    pub env_vars: Vec<EnvVar>,
    /// Service definitions, keyed by service name (the section header), kept in
    /// declaration order so start sequencing and command output are
    /// reproducible (the parser preserves section order end to end).
    pub services: IndexMap<String, Service>,
}

/// An environment variable definition.
///
/// The [`value`](Self::value) is stored verbatim, including any
/// `${service.property}` interpolation placeholders; those are only resolved
/// later by [`resolve_interpolations`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvVar {
    /// Variable name (the part before `=`).
    pub name: String,
    /// Variable value, with interpolation placeholders left intact.
    pub value: String,
}

/// A service definition.
///
/// Every `Service` is guaranteed to have a concrete [`ServiceSource`]: the
/// parser rejects any section that declares no source, so by the time a
/// `Service` exists this invariant holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Commands to run before the service is started
    pub pre_start: Vec<String>,
    /// Commands to run after service is ready
    pub post_start: Vec<String>,
    /// Commands to run before stopping the service
    pub pre_stop: Vec<String>,
    /// Commands to run after the service has stopped
    pub post_stop: Vec<String>,
    /// Health check command
    pub healthcheck: Option<String>,
    /// Timeout in seconds to wait for service to be ready
    pub ready_timeout_secs: Option<u64>,
    /// Build context for Dockerfile builds
    pub build_context: Option<String>,
    /// Command override (replaces the default CMD in the image)
    pub command_override: Option<String>,
}

/// How a service is started.
///
/// Exactly one source per service. There is intentionally no "unset" variant:
/// a section that declares no source is rejected at parse time, so a value of
/// this type always names a real way to start the service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServiceSource {
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortMapping {
    /// Optional name for this port (e.g., "api", "admin")
    pub name: Option<String>,
    /// Container port to expose.
    ///
    /// For an auto-allocated port ([`auto`](Self::auto) is `true`) there is no
    /// container port; this is `0` and eph assigns a free host port at start
    /// time.
    pub container_port: u16,
    /// When `true`, eph allocates a free host port for this mapping at start
    /// time instead of using a fixed number, and tells the spawned process the
    /// assigned port through its environment. Written as `port=auto` /
    /// `port.<name>=auto` in the `.eph` file. Only valid for `run=` services
    /// (the parser rejects it elsewhere), since those are processes eph launches
    /// and can re-launch on a fresh port if they hit a port conflict.
    #[serde(default)]
    pub auto: bool,
}

// ============================================================================
// Parser
// ============================================================================

/// A service section while it is still being parsed.
///
/// The source is optional here because a section accumulates properties line by
/// line and the source may appear on any line (or, erroneously, not at all).
/// [`ServiceBuilder::finish`] turns this into a [`Service`], rejecting sections
/// that never declared a source so the resulting `Service` always has one.
#[derive(Default)]
struct ServiceBuilder {
    name: String,
    source: Option<ServiceSource>,
    ports: Vec<PortMapping>,
    env: HashMap<String, String>,
    volumes: Vec<String>,
    pre_start: Vec<String>,
    post_start: Vec<String>,
    pre_stop: Vec<String>,
    post_stop: Vec<String>,
    healthcheck: Option<String>,
    ready_timeout_secs: Option<u64>,
    build_context: Option<String>,
    command_override: Option<String>,
}

impl ServiceBuilder {
    /// Finalize the section into a [`Service`], requiring a concrete source.
    fn finish(self) -> Result<Service> {
        let source = self.source.ok_or_else(|| {
            anyhow::anyhow!(
                "service '{}' has no source defined (set one of image/dockerfile/compose/run)",
                self.name
            )
        })?;
        // Auto-allocated ports (`port=auto`) only make sense for `run=` services:
        // those are host processes eph launches itself, so it can pick a free
        // port, inject it, and re-launch on a fresh one if the process hits a
        // conflict. For image/dockerfile/compose services Docker already assigns
        // a random host port, and there is no process for eph to relaunch, so a
        // bare `auto` there is a mistake worth catching at parse time.
        if !matches!(source, ServiceSource::Command(_))
            && let Some(p) = self.ports.iter().find(|p| p.auto)
        {
            let which = p
                .name
                .as_deref()
                .map_or_else(|| "port".to_string(), |n| format!("port.{n}"));
            bail!(
                "service '{}' sets `{} = auto`, but auto-allocated ports are only \
                 supported for `run=` services",
                self.name,
                which
            );
        }
        Ok(Service {
            name: self.name,
            source,
            ports: self.ports,
            env: self.env,
            volumes: self.volumes,
            pre_start: self.pre_start,
            post_start: self.post_start,
            pre_stop: self.pre_stop,
            post_stop: self.post_stop,
            healthcheck: self.healthcheck,
            ready_timeout_secs: self.ready_timeout_secs,
            build_context: self.build_context,
            command_override: self.command_override,
        })
    }
}

/// Parse an `.eph` file from a string into an [`EphFile`].
///
/// Top-level `KEY=VALUE` lines become [`EnvVar`]s and `[name]` sections become
/// [`Service`]s. Each returned [`Service`] is guaranteed to carry a concrete
/// [`ServiceSource`], because a section that declares no source is rejected
/// here rather than at runtime.
///
/// # Errors
///
/// Returns an error if:
/// - a line is neither a comment, a section header, nor `KEY=VALUE`
/// - a section header is empty (`[]`)
/// - a service property has an invalid value (e.g. a non-numeric `port`)
/// - an unknown, non-`SCREAMING_SNAKE_CASE` property appears inside a section
///   (a likely typo). An unknown but `SCREAMING_SNAKE_CASE` key is instead
///   reclassified as a trailing top-level variable, with a warning.
/// - a section declares no source (no `image`/`dockerfile`/`compose`/`run`)
///
/// # Examples
///
/// ```
/// # fn main() -> anyhow::Result<()> {
/// let eph = eph::parser::parse("APP_NAME=myapp\n\n[redis]\nimage=redis:7\n")?;
/// assert_eq!(eph.env_vars[0].name, "APP_NAME");
/// assert!(eph.services.contains_key("redis"));
/// # Ok(())
/// # }
/// ```
///
/// A section without a source is rejected:
///
/// ```
/// assert!(eph::parser::parse("[redis]\nport=6379\n").is_err());
/// ```
pub fn parse(input: &str) -> Result<EphFile> {
    let mut env_vars: Vec<EnvVar> = Vec::new();
    // Preserve insertion order of service sections so that finalization (and
    // any error it reports) is deterministic.
    let mut builders: Vec<ServiceBuilder> = Vec::new();
    let mut index_by_name: HashMap<String, usize> = HashMap::new();
    let mut current_service: Option<usize> = None;

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
                bail!("empty section name at line {}", line_num);
            }
            let index = *index_by_name.entry(name.to_string()).or_insert_with(|| {
                builders.push(ServiceBuilder {
                    name: name.to_string(),
                    ..Default::default()
                });
                builders.len() - 1
            });
            current_service = Some(index);
            continue;
        }

        // Parse key=value
        let Some((key, value)) = line.split_once('=') else {
            bail!("invalid syntax at line {}: expected KEY=VALUE", line_num);
        };

        let key = key.trim();
        let value = value.trim();

        // Remove optional quotes from value
        let value = strip_quotes(value);

        if let Some(index) = current_service {
            // We're inside a service section - try to parse as service property
            let service = &mut builders[index];
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
                        key, service.name, line_num
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

    // Finalize each section into a concrete Service, rejecting any that never
    // declared a source. This keeps the illegal "service with no source" state
    // out of the returned EphFile entirely. `builders` is in declaration order,
    // and inserting in that order makes `services` iterate the same way.
    let mut services: IndexMap<String, Service> = IndexMap::with_capacity(builders.len());
    for builder in builders {
        let service = builder.finish()?;
        services.insert(service.name.clone(), service);
    }

    Ok(EphFile { env_vars, services })
}

/// Returns `true` if `key` looks like an environment variable name, i.e. a
/// non-empty `SCREAMING_SNAKE_CASE` identifier: only ASCII uppercase letters,
/// digits, and `_`, starting with an uppercase letter.
///
/// Used by [`parse`] to decide whether an unknown key inside a section is a
/// trailing top-level env var (reclassified, with a warning) or a typo'd
/// service property (a hard error).
fn is_env_var_name(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Strips a single matching pair of surrounding single or double quotes from
/// `s`, returning the inner slice. A string without matching surrounding quotes
/// (or one too short to be quoted) is returned unchanged.
fn strip_quotes(s: &str) -> &str {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parse a port property's value into `(container_port, auto)`.
///
/// The literal `auto` (case-insensitive) requests an eph-allocated free host
/// port and yields `(0, true)`; anything else must be a valid `u16` port number
/// and yields `(n, false)`.
fn parse_port_value(value: &str, line_num: usize) -> Result<(u16, bool)> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok((0, true));
    }
    let port: u16 = value
        .parse()
        .with_context(|| format!("invalid port number at line {}", line_num))?;
    Ok((port, false))
}

fn parse_service_property(
    service: &mut ServiceBuilder,
    key: &str,
    value: &str,
    line_num: usize,
) -> Result<()> {
    match key {
        "image" => service.source = Some(ServiceSource::Image(value.to_string())),
        "dockerfile" => service.source = Some(ServiceSource::Dockerfile(value.to_string())),
        "compose" => service.source = Some(ServiceSource::Compose(value.to_string())),
        // Shell command to run (non-Docker)
        "run" => service.source = Some(ServiceSource::Command(value.to_string())),
        // Container command override (for use with image/dockerfile)
        "command" => service.command_override = Some(value.to_string()),
        "port" => {
            let (port, auto) = parse_port_value(value, line_num)?;
            service.ports.push(PortMapping {
                name: None,
                container_port: port,
                auto,
            });
        }
        "volume" => {
            service.volumes.push(value.to_string());
        }
        "pre-start" => {
            service.pre_start.push(value.to_string());
        }
        "post-start" => {
            service.post_start.push(value.to_string());
        }
        "pre-stop" => {
            service.pre_stop.push(value.to_string());
        }
        "post-stop" => {
            service.post_stop.push(value.to_string());
        }
        "healthcheck" => {
            service.healthcheck = Some(value.to_string());
        }
        "ready-timeout" => {
            let secs: u64 = value
                .parse()
                .with_context(|| format!("invalid timeout at line {}", line_num))?;
            service.ready_timeout_secs = Some(secs);
        }
        key if key.starts_with("port.") => {
            let port_name = &key[5..];
            let (port, auto) = parse_port_value(value, line_num)?;
            service.ports.push(PortMapping {
                name: Some(port_name.to_string()),
                container_port: port,
                auto,
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
                .with_context(|| format!("invalid port number at line {}", line_num))?;
            service.ports.push(PortMapping {
                name: Some(port_name.to_string()),
                container_port: port,
                auto: false,
            });
        }
        // Build context for Dockerfiles
        "context" => {
            service.build_context = Some(value.to_string());
        }
        _ => {
            bail!("unknown service property '{}' at line {}", key, line_num);
        }
    }
    Ok(())
}

// ============================================================================
// Interpolation
// ============================================================================

/// Replaces `${service.property}` interpolations in `input` using `resolver`.
///
/// For each placeholder, `resolver` is called with the `service` and `property`
/// parts. If it returns `Some(value)`, the placeholder is replaced; if it
/// returns `None`, or the placeholder has no `.` separator, the original
/// `${...}` text is left untouched so it can be surfaced unresolved. Text
/// outside placeholders is copied verbatim. This is the resolver used to expand
/// [`EnvVar`] values once services are running.
///
/// # Examples
///
/// A resolved reference is substituted:
///
/// ```
/// use eph::parser::resolve_interpolations;
///
/// let out = resolve_interpolations("redis://localhost:${redis.port}", |svc, prop| {
///     (svc == "redis" && prop == "port").then(|| "6379".to_string())
/// });
/// assert_eq!(out, "redis://localhost:6379");
/// ```
///
/// An unresolved reference is left intact:
///
/// ```
/// use eph::parser::resolve_interpolations;
///
/// let out = resolve_interpolations("${db.port}", |_, _| None);
/// assert_eq!(out, "${db.port}");
/// ```
#[must_use]
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
    fn test_services_preserve_declaration_order() {
        // Section order in the file must survive into `services` iteration, so
        // start sequencing and command output are reproducible rather than
        // varying with hash-map order. Use a name set whose hash order is
        // unlikely to coincide with declaration order.
        let input = r#"
[zebra]
image=busybox

[apple]
image=busybox

[mango]
run=sleep 1
port=auto

[delta]
compose=docker-compose.yml
"#;
        let result = parse(input).unwrap();
        let order: Vec<&str> = result.services.keys().map(String::as_str).collect();
        assert_eq!(order, ["zebra", "apple", "mango", "delta"]);

        // Re-parsing yields the same order every time (a HashMap would not).
        let again = parse(input).unwrap();
        let again_order: Vec<&str> = again.services.keys().map(String::as_str).collect();
        assert_eq!(order, again_order);
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
    fn test_parse_all_lifecycle_hooks() {
        // Each hook accumulates in declaration order, independently of the others.
        let input = r#"
[api]
run=./server
pre-start=go generate ./...
pre-start=sqlc generate
post-start=./scripts/seed.sh
pre-stop=./scripts/drain.sh
post-stop=rm -rf ./tmp/scratch
post-stop=./scripts/deregister.sh
"#;
        let result = parse(input).unwrap();
        let api = result.services.get("api").unwrap();
        assert_eq!(api.pre_start, ["go generate ./...", "sqlc generate"]);
        assert_eq!(api.post_start, ["./scripts/seed.sh"]);
        assert_eq!(api.pre_stop, ["./scripts/drain.sh"]);
        assert_eq!(
            api.post_stop,
            ["rm -rf ./tmp/scratch", "./scripts/deregister.sh"]
        );
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
    fn test_section_without_source_is_rejected_at_parse_time() {
        // A section that declares properties but no source (image/dockerfile/
        // compose/run) is an illegal state that used to parse and only fail at
        // runtime. It must now be rejected by parse() itself.
        let input = r#"
[postgres]
port=5432
env.POSTGRES_USER=dev
"#;
        let err = parse(input).expect_err("section with no source must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("postgres") && msg.contains("no source"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn test_empty_section_is_rejected_at_parse_time() {
        // A bare section header with nothing under it likewise has no source.
        let input = "[redis]\n";
        assert!(parse(input).is_err());
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
    fn test_parse_auto_port_for_run_service() {
        let input = r#"
[web]
run=npm run dev
port=auto
port.hmr=auto
port.api=5000
"#;
        let result = parse(input).unwrap();
        let web = result.services.get("web").unwrap();
        assert!(matches!(&web.source, ServiceSource::Command(c) if c == "npm run dev"));

        let unnamed = web.ports.iter().find(|p| p.name.is_none()).unwrap();
        assert!(unnamed.auto);
        assert_eq!(unnamed.container_port, 0);

        let hmr = web
            .ports
            .iter()
            .find(|p| p.name.as_deref() == Some("hmr"))
            .unwrap();
        assert!(hmr.auto);

        // A fixed numeric port alongside auto ports stays fixed.
        let api = web
            .ports
            .iter()
            .find(|p| p.name.as_deref() == Some("api"))
            .unwrap();
        assert!(!api.auto);
        assert_eq!(api.container_port, 5000);
    }

    #[test]
    fn test_auto_port_is_case_insensitive() {
        let result = parse("[web]\nrun=serve\nport=AUTO\n").unwrap();
        assert!(result.services["web"].ports[0].auto);
    }

    #[test]
    fn test_auto_port_rejected_for_image_service() {
        // `auto` is only meaningful for run= services; Docker already assigns a
        // random host port for image services, so this is a parse-time error.
        let input = "[postgres]\nimage=postgres:16\nport=auto\n";
        let err = parse(input).expect_err("auto port on an image service must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("auto") && msg.contains("run="),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn test_non_auto_invalid_port_still_errors() {
        // A non-numeric, non-`auto` port value remains a hard error.
        assert!(parse("[web]\nrun=serve\nport=nope\n").is_err());
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

    #[test]
    fn resolve_interpolations_passes_through_unresolved_reference() {
        // Arrange: a well-formed `${service.property}` reference whose resolver
        // always declines to resolve it.
        let input = "redis://localhost:${redis.port}/0";

        // Act
        let result = resolve_interpolations(input, |_service, _property| None);

        // Assert: the original placeholder is preserved verbatim, surrounding
        // text included, so the unresolved reference stays visible downstream.
        assert_eq!(result, "redis://localhost:${redis.port}/0");
    }

    #[test]
    fn strip_quotes_removes_matching_double_quotes() {
        // Arrange
        let input = "\"hello\"";

        // Act
        let result = strip_quotes(input);

        // Assert
        assert_eq!(result, "hello");
    }

    #[test]
    fn strip_quotes_removes_matching_single_quotes() {
        assert_eq!(strip_quotes("'hello'"), "hello");
    }

    #[test]
    fn strip_quotes_leaves_unquoted_value_unchanged() {
        assert_eq!(strip_quotes("hello"), "hello");
    }

    #[test]
    fn strip_quotes_leaves_mismatched_quotes_unchanged() {
        // A leading quote without a matching trailing quote is not stripped.
        assert_eq!(strip_quotes("\"hello"), "\"hello");
        assert_eq!(strip_quotes("'hello\""), "'hello\"");
    }

    #[test]
    fn is_env_var_name_accepts_screaming_snake_case() {
        // Arrange / Act / Assert
        assert!(is_env_var_name("DATABASE_URL"));
        assert!(is_env_var_name("LOG_LEVEL_2"));
        assert!(is_env_var_name("A"));
    }

    #[test]
    fn is_env_var_name_rejects_non_env_keys() {
        // Empty, lowercase, leading digit, and dotted property keys are not
        // env-var names, so unknown such keys stay hard parse errors.
        assert!(!is_env_var_name(""));
        assert!(!is_env_var_name("port"));
        assert!(!is_env_var_name("post-start"));
        assert!(!is_env_var_name("2FOO"));
        assert!(!is_env_var_name("env.FOO"));
    }
}
