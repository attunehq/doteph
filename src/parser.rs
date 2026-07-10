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
//! # After the first service section, top-level variables live in an [env]
//! # section. They can interpolate service properties.
//! [env]
//! DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/app
//! ```
//!
//! # Where variables may appear
//!
//! Top-level environment variables (what `eph env` emits) are declared either
//! above the first section or inside an `[env]` section, which may appear any
//! number of times. A bare `KEY=VALUE` after a service section is a parse
//! error: sections do not end at blank lines, so the parser would otherwise
//! have to guess whether the key was a service property or a variable. The
//! old parser guessed (any unknown `SCREAMING_SNAKE_CASE` key silently ended
//! the section); this one refuses and tells you where the variable belongs.

use anyhow::{Context as _, Result, bail};
use indexmap::IndexMap;
use serde::Serialize;
use std::collections::HashMap;
use std::num::{NonZeroU16, NonZeroU64};

// ============================================================================
// AST Types
// ============================================================================

/// A parsed `.eph` file.
///
/// Produced by [`parse`]. Holds the top-level [`EnvVar`]s and the named
/// [`Service`] definitions extracted from the file.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EphFile {
    /// Top-level environment variables, in declaration order. Declared above
    /// the first section or inside `[env]` sections; both spellings land here.
    pub env_vars: Vec<EnvVar>,
    /// Service definitions, keyed by service name (the section header), kept in
    /// declaration order so start sequencing and command output are
    /// reproducible (the parser preserves section order end to end).
    pub services: IndexMap<String, Service>,
    /// The role dependency graph, when the file uses roles.
    ///
    /// `None` in "legacy mode" (no service declares a `role=` and there is no
    /// `roles_order`), where ordering falls back to declaration order with `run=`
    /// services last. `Some` in "roles mode", where it is the single source of
    /// truth for bring-up order: services are grouped by role, roles are brought
    /// up in topological order of this graph, and teardown is the exact reverse.
    /// The parser guarantees the graph is consistent with the services (every
    /// service role is a node, every node has at least one service, every edge
    /// points at a known role, no cycles).
    pub roles_order: Option<RolesOrder>,
}

/// The role dependency graph for a `.eph` file in "roles mode".
///
/// Written either as a linear top-level `roles_order=a,b,c` (sugar: `b` depends
/// on `a`, `c` on `b`) or as a `[roles_order]` section giving each role's
/// dependencies explicitly (`app=dep,cache`, a bare `dep=` for a root). Both
/// desugar to this adjacency list. "Depends on" means "must come up first": an
/// edge `app -> dep` orders `dep` before `app` and pulls `dep` in whenever `app`
/// is requested.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RolesOrder {
    /// Each role mapped to the roles it depends on. Keys are every role in the
    /// graph (roots included, with an empty dependency list), kept in declaration
    /// order so the topological sort is a deterministic, stable tie-break.
    pub deps: IndexMap<String, Vec<String>>,
}

impl RolesOrder {
    /// The roles in topological order: every role appears after all the roles it
    /// depends on. Ties (roles with no ordering constraint between them) break by
    /// declaration order, so the result is deterministic.
    ///
    /// # Errors
    ///
    /// Returns an error naming a role involved in a dependency cycle. The parser
    /// calls this during validation, so a `RolesOrder` that escaped parsing is
    /// always acyclic and this cannot fail at runtime.
    pub fn topo_roles(&self) -> Result<Vec<String>> {
        let mut ordered: Vec<String> = Vec::with_capacity(self.deps.len());
        // Kahn-style, but scanning in declaration order each round so ties break
        // deterministically. n is tiny (a handful of roles), so the simple
        // O(n^2) scan is not worth optimizing.
        while ordered.len() < self.deps.len() {
            let next = self.deps.keys().find(|role| {
                !ordered.contains(*role) && self.deps[*role].iter().all(|dep| ordered.contains(dep))
            });
            match next {
                Some(role) => ordered.push(role.clone()),
                None => {
                    let stuck: Vec<&str> = self
                        .deps
                        .keys()
                        .filter(|r| !ordered.contains(*r))
                        .map(String::as_str)
                        .collect();
                    bail!(
                        "roles_order has a dependency cycle among: {}",
                        stuck.join(", ")
                    );
                }
            }
        }
        Ok(ordered)
    }

    /// The transitive closure of `roles` over their dependencies: every requested
    /// role plus everything it (transitively) depends on. This is the set brought
    /// up by `eph up --role=<role>`, since a role cannot run without the roles it
    /// depends on.
    ///
    /// # Errors
    ///
    /// Returns an error if a requested role is not part of the graph.
    pub fn forward_closure(&self, roles: &[String]) -> Result<Vec<String>> {
        self.closure(roles, |role| {
            self.deps.get(role).cloned().unwrap_or_default()
        })
    }

    /// The transitive closure of `roles` over their dependents: every requested
    /// role plus everything that (transitively) depends on it. This is the set
    /// torn down by `eph down --role=<role>`, since a dependency cannot be removed
    /// while the roles that need it are still up.
    ///
    /// # Errors
    ///
    /// Returns an error if a requested role is not part of the graph.
    pub fn reverse_closure(&self, roles: &[String]) -> Result<Vec<String>> {
        self.closure(roles, |role| {
            self.deps
                .iter()
                .filter(|(_, deps)| deps.iter().any(|d| d == role))
                .map(|(r, _)| r.clone())
                .collect()
        })
    }

    /// Shared transitive-closure walk. `neighbors` yields the roles to follow
    /// from a given role (its dependencies for the forward closure, its
    /// dependents for the reverse). Validates that every seed role exists.
    fn closure<F>(&self, roles: &[String], neighbors: F) -> Result<Vec<String>>
    where
        F: Fn(&str) -> Vec<String>,
    {
        for role in roles {
            if !self.deps.contains_key(role) {
                bail!(
                    "unknown role '{}' (known roles: {})",
                    role,
                    self.deps.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
        }
        let mut seen: Vec<String> = Vec::new();
        let mut stack: Vec<String> = roles.to_vec();
        while let Some(role) = stack.pop() {
            if seen.contains(&role) {
                continue;
            }
            seen.push(role.clone());
            stack.extend(neighbors(&role));
        }
        Ok(seen)
    }
}

impl EphFile {
    /// The order services are brought up in.
    ///
    /// In roles mode this is the topological order of the role graph (roles
    /// grouped, dependencies first), with services inside a role kept in
    /// declaration order. In legacy mode (no roles) it is declaration order with
    /// `run=` services deferred to the end, so a managed app starts after the
    /// backing services it references. Teardown is the exact reverse either way.
    ///
    /// This is the single source of truth for start sequencing across the
    /// codebase; `service.rs` calls it rather than re-deriving order.
    #[must_use]
    pub fn start_order(&self) -> Vec<&String> {
        match &self.roles_order {
            Some(order) => {
                // Safe to unwrap: the parser rejected cycles, so a parsed
                // `EphFile` always has an acyclic graph.
                let topo = order
                    .topo_roles()
                    .expect("roles_order validated acyclic at parse time");
                let mut names: Vec<&String> = Vec::with_capacity(self.services.len());
                for role in &topo {
                    for (name, svc) in &self.services {
                        if svc.role.as_deref() == Some(role.as_str()) {
                            names.push(name);
                        }
                    }
                }
                names
            }
            None => {
                let mut names: Vec<&String> = self.services.keys().collect();
                names.sort_by_key(|name| {
                    matches!(self.services[*name].source, ServiceSource::Command(_))
                });
                names
            }
        }
    }

    /// The service names to bring up for `eph up --role=<roles>`: every service
    /// whose role is in the forward (dependency) closure of `roles`, returned in
    /// bring-up order.
    ///
    /// # Errors
    ///
    /// Returns an error if the file does not use roles, or if a requested role is
    /// not defined.
    pub fn services_for_roles_up(&self, roles: &[String]) -> Result<Vec<String>> {
        let order = self.roles_order.as_ref().context(
            "this .eph file does not define roles, so `--role` cannot be used; \
             pass service names instead, or add a `roles_order`",
        )?;
        let role_set = order.forward_closure(roles)?;
        Ok(self.services_in_role_set(&role_set))
    }

    /// The service names to tear down for `eph down --role=<roles>`: every service
    /// whose role is in the reverse (dependent) closure of `roles`, returned in
    /// bring-up order (the caller stops in reverse).
    ///
    /// # Errors
    ///
    /// Returns an error if the file does not use roles, or if a requested role is
    /// not defined.
    pub fn services_for_roles_down(&self, roles: &[String]) -> Result<Vec<String>> {
        let order = self.roles_order.as_ref().context(
            "this .eph file does not define roles, so `--role` cannot be used; \
             pass service names instead, or add a `roles_order`",
        )?;
        let role_set = order.reverse_closure(roles)?;
        Ok(self.services_in_role_set(&role_set))
    }

    /// Service names whose role is in `role_set`, in bring-up order.
    fn services_in_role_set(&self, role_set: &[String]) -> Vec<String> {
        self.start_order()
            .into_iter()
            .filter(|name| {
                self.services[*name]
                    .role
                    .as_ref()
                    .is_some_and(|r| role_set.contains(r))
            })
            .cloned()
            .collect()
    }
}

/// An environment variable definition.
///
/// The [`value`](Self::value) is stored verbatim, including any
/// `${service.property}` interpolation placeholders; those are only resolved
/// later by [`resolve_interpolations`].
#[derive(Debug, Clone, PartialEq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Service {
    /// Service name (matches section header)
    pub name: String,
    /// The role this service belongs to, if any (`role=` in the `.eph` file).
    ///
    /// `None` means the service is unclassified. A file with no roles anywhere
    /// (and no `roles_order`) behaves exactly as it did before roles existed:
    /// declaration order, `run=` services last. Once any service declares a role
    /// the file is in "roles mode", where every service must have a role that
    /// appears in [`EphFile::roles_order`] and that ordering drives start
    /// sequencing instead of the source-based heuristic. The parser enforces this
    /// invariant, so a `Service` seen at runtime is either wholly unclassified or
    /// part of a fully specified role graph.
    pub role: Option<String>,
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
    /// Readiness probe and its optional timeout.
    ///
    /// Keeping the timeout with the probe prevents a parsed service from
    /// carrying a timeout that no startup path could use.
    pub healthcheck: Option<Healthcheck>,
    /// Build context for Dockerfile builds
    pub build_context: Option<String>,
    /// Parsed command override (replaces the default CMD in the image).
    pub command_override: Option<CommandOverride>,
}

/// A healthcheck command with an optional non-zero readiness timeout.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Healthcheck {
    /// Shell command polled until it succeeds.
    pub command: String,
    /// Explicit timeout. Backends use their existing default when absent.
    pub timeout_secs: Option<NonZeroU64>,
}

/// A `command=` override that has already been split into a non-empty argv.
///
/// The field is private so malformed shell quoting and empty commands can only
/// enter through the `.eph` parser.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandOverride(Vec<String>);

impl CommandOverride {
    fn parse(value: &str, service: &str) -> Result<Self> {
        let argv = shell_words::split(value).map_err(|error| {
            anyhow::anyhow!(
                "invalid command override for service '{}': {}",
                service,
                error
            )
        })?;
        if argv.first().is_none_or(String::is_empty) {
            bail!(
                "invalid command override for service '{}': command must name an executable",
                service
            );
        }
        Ok(Self(argv))
    }

    /// Return the parsed argv without reparsing user input at startup.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.0
    }
}

/// How a service is started.
///
/// Exactly one source per service. There is intentionally no "unset" variant:
/// a section that declares no source is rejected at parse time, so a value of
/// this type always names a real way to start the service.
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// A port declaration whose shape records how eph resolves it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum PortMapping {
    /// A fixed port for an image, Dockerfile, or `run=` service.
    Fixed {
        /// Optional interpolation name such as `api`.
        name: Option<String>,
        /// Non-zero container or host-process port.
        port: NonZeroU16,
    },
    /// A host port eph allocates before launching a `run=` service.
    Auto {
        /// Optional interpolation name such as `hmr`.
        name: Option<String>,
    },
    /// A published port belonging to one service in a Compose project.
    Compose {
        /// User-facing interpolation alias.
        alias: String,
        /// Service name passed to `docker compose port`.
        service: String,
        /// Non-zero container port passed to `docker compose port`.
        port: NonZeroU16,
    },
}

impl PortMapping {
    /// User-facing name, or `None` for an unnamed direct mapping.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Fixed { name, .. } | Self::Auto { name } => name.as_deref(),
            Self::Compose { alias, .. } => Some(alias),
        }
    }

    /// Runtime/interpolation key, using `default` for an unnamed direct port.
    #[must_use]
    pub fn runtime_name(&self) -> &str {
        self.name().unwrap_or("default")
    }

    /// Fixed container port. Auto mappings have no fixed port.
    #[must_use]
    pub fn container_port(&self) -> Option<u16> {
        match self {
            Self::Fixed { port, .. } | Self::Compose { port, .. } => Some(port.get()),
            Self::Auto { .. } => None,
        }
    }

    /// Whether eph must allocate this mapping at process startup.
    #[must_use]
    pub fn is_auto(&self) -> bool {
        matches!(self, Self::Auto { .. })
    }

    /// Compose service name, when this is a Compose mapping.
    #[must_use]
    pub fn compose_service(&self) -> Option<&str> {
        match self {
            Self::Compose { service, .. } => Some(service),
            Self::Fixed { .. } | Self::Auto { .. } => None,
        }
    }
}

// ============================================================================
// Parser
// ============================================================================

/// The names of every property a service section accepts, for error messages.
const KNOWN_PROPERTIES: &str = "image, dockerfile, compose, run, role, command, port, \
     port.<name>, expose.<name>, env.<KEY>, volume, pre-start, post-start, pre-stop, \
     post-stop, healthcheck, ready-timeout, context";

/// What the parser is currently reading: top-of-file variables, an `[env]`
/// section, the `[roles_order]` section, or a service section.
#[derive(Clone, Copy)]
enum Context {
    /// Before the first section: bare `KEY=VALUE` lines are top-level variables.
    TopLevel,
    /// Inside an `[env]` section: every line is a top-level variable.
    Env,
    /// Inside the `[roles_order]` section: every line is a `role=deps` edge.
    RolesOrder,
    /// Inside the service section at this index in the builders list.
    Service(usize),
}

/// A `${service.property}` reference found while parsing, kept for validation
/// once every section has been read (so forward references work).
struct PlaceholderRef {
    service: String,
    property: String,
    line: usize,
    /// Where the reference appeared, for the error message (e.g. "value of
    /// DATABASE_URL" or "env.PORT of service 'web'").
    context: String,
}

/// A service section while it is still being parsed.
///
/// The source is optional here because a section accumulates properties line by
/// line and the source may appear on any line (or, erroneously, not at all).
/// [`ServiceBuilder::finish`] turns this into a [`Service`], rejecting sections
/// that never declared a source so the resulting `Service` always has one.
#[derive(Default)]
struct ServiceBuilder {
    name: String,
    /// The line the section header appeared on, for duplicate-section errors.
    first_line: usize,
    role: Option<String>,
    source: Option<ServiceSource>,
    ports: Vec<PortMapping>,
    /// Ports declared with `expose.<name>=`, kept separate from `ports` until
    /// [`finish`](Self::finish) so the parser can enforce that `expose.` is used
    /// with `compose=` services and `port=`/`port.<name>=` with everything else.
    expose: Vec<PortMapping>,
    env: HashMap<String, String>,
    volumes: Vec<String>,
    pre_start: Vec<String>,
    post_start: Vec<String>,
    pre_stop: Vec<String>,
    post_stop: Vec<String>,
    healthcheck: Option<String>,
    ready_timeout_secs: Option<NonZeroU64>,
    build_context: Option<String>,
    command_override: Option<CommandOverride>,
}

impl ServiceBuilder {
    /// Finalize the section into a [`Service`], requiring a concrete source and
    /// enforcing the property/source pairings that only make sense together.
    fn finish(mut self) -> Result<Service> {
        let source = self.source.take().ok_or_else(|| {
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
            && let Some(p) = self.ports.iter().find(|p| p.is_auto())
        {
            let which = p
                .name()
                .map_or_else(|| "port".to_string(), |n| format!("port.{n}"));
            bail!(
                "service '{}' sets `{} = auto`, but auto-allocated ports are only \
                 supported for `run=` services",
                self.name,
                which
            );
        }
        // `command=` overrides an image's CMD; a `run=` service's command IS its
        // source and a compose service's command lives in the compose file, so on
        // those a `command=` would silently do nothing. Reject it instead.
        if self.command_override.is_some()
            && !matches!(
                source,
                ServiceSource::Image(_) | ServiceSource::Dockerfile(_)
            )
        {
            bail!(
                "service '{}' sets `command=`, which only applies to image= or \
                 dockerfile= services (a run= service's command is the run= value \
                 itself; a compose service's command belongs in the compose file)",
                self.name
            );
        }
        if self.build_context.is_some() && !matches!(source, ServiceSource::Dockerfile(_)) {
            bail!(
                "service '{}' sets `context=`, which only applies to dockerfile= services",
                self.name
            );
        }
        if !self.volumes.is_empty()
            && !matches!(
                source,
                ServiceSource::Image(_) | ServiceSource::Dockerfile(_)
            )
        {
            bail!(
                "service '{}' sets `volume=`, which only applies to image= or dockerfile= services",
                self.name
            );
        }
        if self.ready_timeout_secs.is_some() && self.healthcheck.is_none() {
            bail!(
                "service '{}' sets `ready-timeout=` without a `healthcheck=` to time",
                self.name
            );
        }
        // `expose.<name>=` names ports of services inside a compose project;
        // `port=`/`port.<name>=` declare ports eph itself publishes. Each is
        // meaningless for the other backend, so a mix-up is caught here.
        let ports = if matches!(source, ServiceSource::Compose(_)) {
            if let Some(p) = self.ports.first() {
                let which = p
                    .name()
                    .map_or_else(|| "port".to_string(), |n| format!("port.{n}"));
                bail!(
                    "compose service '{}' declares `{}`; compose services name \
                     their ports with `expose.<name>=<container-port>` instead",
                    self.name,
                    which
                );
            }
            self.expose
        } else {
            if let Some(p) = self.expose.first() {
                bail!(
                    "service '{}' declares `expose.{}`, which only applies to \
                     compose= services; use `port=` or `port.<name>=` instead",
                    self.name,
                    p.name().unwrap_or_default()
                );
            }
            self.ports
        };
        Ok(Service {
            name: self.name,
            role: self.role,
            source,
            ports,
            env: self.env,
            volumes: self.volumes,
            pre_start: self.pre_start,
            post_start: self.post_start,
            pre_stop: self.pre_stop,
            post_stop: self.post_stop,
            healthcheck: self.healthcheck.map(|command| Healthcheck {
                command,
                timeout_secs: self.ready_timeout_secs,
            }),
            build_context: self.build_context,
            command_override: self.command_override,
        })
    }

    /// True if a port with this name (or an unnamed port, for `None`) has
    /// already been declared via either `port` or `expose`.
    fn has_port_named(&self, name: Option<&str>) -> bool {
        let runtime_name = name.unwrap_or("default");
        self.ports
            .iter()
            .chain(self.expose.iter())
            .any(|port| port.runtime_name() == runtime_name)
    }
}

/// Parse an `.eph` file from a string into an [`EphFile`].
///
/// Top-level `KEY=VALUE` lines (above the first section, or inside an `[env]`
/// section) become [`EnvVar`]s and `[name]` sections become [`Service`]s. Each
/// returned [`Service`] is guaranteed to carry a concrete [`ServiceSource`],
/// because a section that declares no source is rejected here rather than at
/// runtime. A leading UTF-8 byte-order mark is ignored.
///
/// # Errors
///
/// Returns an error if:
/// - a line is neither a comment, a section header, nor `KEY=VALUE`
/// - a section name is empty, is not `[a-z][a-z0-9-]*`, or repeats an earlier
///   section
/// - a service property is unknown, duplicated (for single-valued properties),
///   or has an invalid or empty value
/// - a bare `KEY=VALUE` appears after a service section (move it into `[env]`)
/// - a top-level variable name is not a valid environment variable name, or is
///   declared twice
/// - an interpolation is malformed, references an unknown service or port,
///   uses an unknown property, or uses bare `.port` for a service without
///   exactly one port
/// - a section declares no source (no `image`/`dockerfile`/`compose`/`run`)
/// - a property is incompatible with its service source
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
/// Variables after a service section live in an `[env]` section:
///
/// ```
/// # fn main() -> anyhow::Result<()> {
/// let eph = eph::parser::parse(
///     "[redis]\nimage=redis:7\nport=6379\n\n[env]\nREDIS_URL=redis://localhost:${redis.port}\n",
/// )?;
/// assert_eq!(eph.env_vars[0].name, "REDIS_URL");
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
    // Windows editors love to prepend a UTF-8 BOM; without this it would end up
    // glued to the first variable's name and break the emitted `export` line.
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);

    let mut env_vars: Vec<EnvVar> = Vec::new();
    // Top-level variable names already seen, for duplicate detection. The
    // top-of-file block and every [env] section share one namespace.
    let mut env_lines: HashMap<String, usize> = HashMap::new();
    // Preserve insertion order of service sections so that finalization (and
    // any error it reports) is deterministic.
    let mut builders: Vec<ServiceBuilder> = Vec::new();
    let mut index_by_name: HashMap<String, usize> = HashMap::new();
    let mut context = Context::TopLevel;
    // Every ${service.property} reference seen, validated against the full
    // service list after the whole file is read so forward references work.
    let mut refs: Vec<PlaceholderRef> = Vec::new();

    // The role graph, accumulated from whichever form the file uses. The linear
    // `roles_order=a,b,c` key and the `[roles_order]` section are mutually
    // exclusive; `roles_order_dag` holds the section form's adjacency list.
    let mut roles_order_linear: Option<Vec<String>> = None;
    let mut roles_order_dag: Option<IndexMap<String, Vec<String>>> = None;

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
            // `[roles_order]` is a reserved section, not a service: its lines are
            // `role=dependencies` edges rather than service properties.
            if name == "roles_order" {
                if roles_order_linear.is_some() {
                    bail!(
                        "line {}: cannot use both a top-level `roles_order=` and a \
                         [roles_order] section; pick one",
                        line_num
                    );
                }
                roles_order_dag.get_or_insert_with(IndexMap::new);
                context = Context::RolesOrder;
                continue;
            }
            // `[env]` is a reserved section for top-level variables. It may
            // repeat: each occurrence just switches back to variable context, so
            // variables can be grouped near the services they describe.
            if name == "env" {
                context = Context::Env;
                continue;
            }
            if let Some(canonical) = reserved_section_hint(name) {
                bail!(
                    "unknown section [{}] at line {}; did you mean [{}]?",
                    name,
                    line_num,
                    canonical
                );
            }
            if !is_valid_service_name(name) {
                bail!(
                    "invalid service name '{}' at line {}: service names are \
                     lowercase letters, digits, and hyphens, starting with a letter \
                     (they become container names, ${{name.property}} references, \
                     and EPH_<NAME>_* variables)",
                    name,
                    line_num
                );
            }
            if let Some(&existing) = index_by_name.get(name) {
                bail!(
                    "duplicate section [{}] at line {} (first defined at line {})",
                    name,
                    line_num,
                    builders[existing].first_line
                );
            }
            builders.push(ServiceBuilder {
                name: name.to_string(),
                first_line: line_num,
                ..Default::default()
            });
            index_by_name.insert(name.to_string(), builders.len() - 1);
            context = Context::Service(builders.len() - 1);
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

        match context {
            // Inside `[roles_order]`, every line is a `role=dep1,dep2` edge (an
            // empty value declares a root that depends on nothing).
            Context::RolesOrder => {
                let dag = roles_order_dag
                    .as_mut()
                    .expect("dag is initialized on entering [roles_order]");
                if key.is_empty() {
                    bail!("line {}: empty role name in [roles_order]", line_num);
                }
                if dag.contains_key(key) {
                    bail!(
                        "line {}: duplicate role '{}' in [roles_order]",
                        line_num,
                        key
                    );
                }
                dag.insert(
                    key.to_string(),
                    split_role_dependencies(value, key, line_num)?,
                );
            }

            // Top-of-file: bare variables, plus the linear `roles_order=` form.
            Context::TopLevel if key == "roles_order" => {
                if roles_order_dag.is_some() {
                    bail!(
                        "line {}: cannot use both a top-level `roles_order=` and a \
                         [roles_order] section; pick one",
                        line_num
                    );
                }
                if roles_order_linear.is_some() {
                    bail!("line {}: duplicate top-level `roles_order=`", line_num);
                }
                roles_order_linear = Some(split_roles_checked(value, line_num)?);
            }

            Context::TopLevel | Context::Env => {
                if key == "roles_order" {
                    // Only reachable in Context::Env thanks to the arm above.
                    bail!(
                        "line {}: `roles_order=` is reserved and cannot be declared \
                         inside [env]; declare it above the first section",
                        line_num
                    );
                }
                if !is_valid_env_name(key) {
                    bail!(
                        "invalid environment variable name '{}' at line {}: names \
                         are letters, digits, and underscores, not starting with a \
                         digit (the shell rejects anything else at `eval` time)",
                        key,
                        line_num
                    );
                }
                reject_reserved_env_name(key, line_num, "top-level environment")?;
                if let Some(first) = env_lines.get(key) {
                    bail!(
                        "duplicate environment variable '{}' at line {} (first \
                         defined at line {})",
                        key,
                        line_num,
                        first
                    );
                }
                scan_placeholders(value, line_num, &format!("the value of {}", key), &mut refs)?;
                env_lines.insert(key.to_string(), line_num);
                env_vars.push(EnvVar {
                    name: key.to_string(),
                    value: value.to_string(),
                });
            }

            Context::Service(index) => {
                let service = &mut builders[index];
                parse_service_property(service, key, value, line_num, &mut refs)?;
            }
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

    // Every ${service.property} must name a defined service and a property that
    // the service actually exposes. Checked after the whole file is read so
    // forward references work without weakening the returned model.
    for r in &refs {
        validate_placeholder(r, &services)?;
    }

    // Collapse the two spellings into one graph: the linear form desugars into an
    // adjacency list (each role depends on the one before it), the section form
    // is already one. Then check the graph and the services agree before handing
    // back an `EphFile`, so "roles mode" is either fully specified or absent.
    let roles_order = match (roles_order_linear, roles_order_dag) {
        (Some(linear), None) => Some(RolesOrder {
            deps: desugar_linear_order(&linear),
        }),
        (None, Some(deps)) => Some(RolesOrder { deps }),
        (None, None) => None,
        // The parse loop rejects declaring both, so this pair cannot occur.
        (Some(_), Some(_)) => unreachable!("both roles_order forms present"),
    };
    validate_roles(&services, roles_order.as_ref())?;

    Ok(EphFile {
        env_vars,
        services,
        roles_order,
    })
}

/// Parse a non-empty comma-separated role list without discarding typo-shaped
/// empty or duplicate segments.
fn split_roles_checked(value: &str, line_num: usize) -> Result<Vec<String>> {
    split_role_list(value, line_num, "roles_order", false)
}

/// Parse one `[roles_order]` dependency list. A wholly empty value declares a
/// root role; commas still denote segments and therefore cannot be empty.
fn split_role_dependencies(value: &str, role: &str, line_num: usize) -> Result<Vec<String>> {
    split_role_list(
        value,
        line_num,
        &format!("dependencies of role '{}'", role),
        true,
    )
}

fn split_role_list(
    value: &str,
    line_num: usize,
    context: &str,
    allow_empty_list: bool,
) -> Result<Vec<String>> {
    if value.trim().is_empty() {
        if allow_empty_list {
            return Ok(Vec::new());
        }
        bail!("line {}: {} must list at least one role", line_num, context);
    }

    let mut roles = Vec::new();
    for segment in value.split(',') {
        let role = segment.trim();
        if role.is_empty() {
            bail!("line {}: empty role in {}", line_num, context);
        }
        if roles.iter().any(|existing| existing == role) {
            bail!(
                "line {}: duplicate role '{}' in {}",
                line_num,
                role,
                context
            );
        }
        roles.push(role.to_string());
    }
    Ok(roles)
}

/// Desugar the linear `roles_order=a,b,c` form into an adjacency list: `a` is a
/// root, and every later role depends on the one immediately before it, so the
/// chain topologically sorts back to the written order and `--role=c` pulls in
/// `a` and `b`. Declaration order is preserved in the returned map.
fn desugar_linear_order(roles: &[String]) -> IndexMap<String, Vec<String>> {
    let mut deps = IndexMap::with_capacity(roles.len());
    let mut prev: Option<&String> = None;
    for role in roles {
        let edges = prev.map(|p| vec![p.clone()]).unwrap_or_default();
        deps.insert(role.clone(), edges);
        prev = Some(role);
    }
    deps
}

/// Enforce the "roles mode" invariants tying the role graph to the services.
///
/// A file is in roles mode when any service declares a `role=` or a `roles_order`
/// is present; the two must then be fully consistent. This is where the mutual
/// completeness the format promises is checked, so nothing downstream has to
/// cope with a half-specified graph:
///
/// - a `roles_order` requires every service to declare a role, and vice versa;
/// - every service role, and every dependency edge, names a role in the graph;
/// - every role in the graph has at least one service; and
/// - the graph is acyclic.
///
/// In legacy mode (no roles anywhere) there is nothing to check.
fn validate_roles(
    services: &IndexMap<String, Service>,
    roles_order: Option<&RolesOrder>,
) -> Result<()> {
    let any_role = services.values().any(|s| s.role.is_some());
    let Some(order) = roles_order else {
        // No graph. Legal only if no service is tagged either.
        if any_role {
            bail!(
                "services declare a `role=` but the file has no `roles_order`; add a \
                 top-level `roles_order=...` or a [roles_order] section listing the roles"
            );
        }
        return Ok(());
    };

    if order.deps.is_empty() {
        bail!("roles_order is empty; list at least one role");
    }

    // Every service must be tagged, and with a role the graph knows.
    for service in services.values() {
        let Some(role) = &service.role else {
            bail!(
                "service '{}' has no `role=`, but this file uses `roles_order`; every \
                 service must declare a role when roles_order is set",
                service.name
            );
        };
        if !order.deps.contains_key(role) {
            bail!(
                "service '{}' has role '{}', which is not listed in roles_order (known \
                 roles: {})",
                service.name,
                role,
                order.deps.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
    }

    // Every edge must point at a known role, and every role must be backed by at
    // least one service (an empty role is almost certainly a typo).
    for (role, deps) in &order.deps {
        for dep in deps {
            if !order.deps.contains_key(dep) {
                bail!(
                    "roles_order: role '{}' depends on unknown role '{}'",
                    role,
                    dep
                );
            }
        }
        if !services
            .values()
            .any(|s| s.role.as_deref() == Some(role.as_str()))
        {
            bail!(
                "roles_order lists role '{}', but no service declares it",
                role
            );
        }
    }

    // Reject cycles up front so `topo_roles` is infallible everywhere else.
    order.topo_roles()?;

    Ok(())
}

/// Returns `true` if `name` is a legal service name: lowercase ASCII letters,
/// digits, and hyphens, starting with a letter.
///
/// The rule is deliberately strict because a service name leaks into several
/// other namespaces: Docker container names, `${name.property}` interpolation
/// (where a `.` would be split as a property separator), and `EPH_<NAME>_*`
/// metadata variables (where uppercasing and `-` -> `_` mapping must stay
/// collision-free; allowing both `-` and `_` would let `auth-db` and `auth_db`
/// collide).
fn is_valid_service_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_valid_compose_service_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Returns `true` if `name` is a valid environment variable name for the
/// shells `eph env` targets: letters, digits, and underscores, not starting
/// with a digit. Anything else would make the emitted `export NAME=...` line
/// fail at `eval` time.
fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn reject_reserved_env_name(name: &str, line_num: usize, context: &str) -> Result<()> {
    if name
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("EPH_"))
    {
        bail!(
            "reserved environment variable '{}' in {} at line {}: EPH_* names are managed by eph",
            name,
            context,
            line_num
        );
    }
    Ok(())
}

/// If `name` looks like a misspelling of a reserved section, the canonical
/// name to suggest. Normalizes case and hyphens so `[Role-Order]` and
/// `[roles-order]` both point at `[roles_order]`.
fn reserved_section_hint(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().replace('-', "_").as_str() {
        "roles_order" | "role_order" | "rolesorder" | "roleorder" | "roles" => Some("roles_order"),
        "env" | "envs" | "environment" | "variables" | "vars" => Some("env"),
        _ => None,
    }
}

/// Strips a single matching pair of surrounding single or double quotes from
/// `s`, returning the inner slice.
///
/// The pair is only stripped when the result is unambiguous: the string is at
/// least two characters, starts and ends with the same quote, and contains no
/// further occurrence of that quote in between. A value like `"a" and "b"` is
/// returned unchanged rather than mangled to `a" and "b`, and a bare `"` (too
/// short to be a pair) is returned as-is.
fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let q = bytes[0];
        if (q == b'"' || q == b'\'')
            && bytes[bytes.len() - 1] == q
            && !s[1..s.len() - 1].contains(q as char)
        {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Parse the `${service.property}` placeholders in an environment value,
/// recording each reference for post-parse semantic checks.
///
/// Rejects an unterminated `${` and any placeholder that is not of the
/// two-part `service.property` form. A literal `${` can be written as `$${`;
/// [`resolve_interpolations`] collapses that escape when the value is
/// rendered. `context` describes where the value came from, for error
/// messages.
fn scan_placeholders(
    value: &str,
    line: usize,
    context: &str,
    refs: &mut Vec<PlaceholderRef>,
) -> Result<()> {
    let mut i = 0;
    while i < value.len() {
        let rest = &value[i..];
        if rest.starts_with("$${") {
            i += 3;
            continue;
        }
        if let Some(tail) = rest.strip_prefix("${") {
            let Some(end) = tail.find('}') else {
                bail!(
                    "unterminated '${{' in {} at line {}; close it with '}}', or \
                     write a literal '${{' as '$${{'",
                    context,
                    line
                );
            };
            let content = &tail[..end];
            match content.split_once('.') {
                Some((service, property)) if !service.is_empty() && !property.is_empty() => {
                    refs.push(PlaceholderRef {
                        service: service.to_string(),
                        property: property.to_string(),
                        line,
                        context: context.to_string(),
                    });
                }
                _ => bail!(
                    "invalid interpolation '${{{}}}' in {} at line {}: expected \
                     ${{service.property}} (e.g. ${{postgres.port}}); write a \
                     literal '${{' as '$${{'",
                    content,
                    context,
                    line
                ),
            }
            i += 2 + end + 1;
            continue;
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    Ok(())
}

/// Record eph's dotted placeholders inside a shell command without claiming
/// ordinary shell parameter expansion such as `${PORT}` or `${HOME:-/tmp}`.
fn scan_command_placeholders(
    value: &str,
    line: usize,
    context: &str,
    refs: &mut Vec<PlaceholderRef>,
) {
    let mut offset = 0;
    while offset < value.len() {
        let rest = &value[offset..];
        if rest.starts_with("$${") {
            offset += 3;
            continue;
        }
        if let Some(tail) = rest.strip_prefix("${") {
            let Some(end) = tail.find('}') else {
                break;
            };
            let content = &tail[..end];
            if let Some((service, property)) = content.split_once('.')
                && !service.is_empty()
                && !property.is_empty()
            {
                refs.push(PlaceholderRef {
                    service: service.to_string(),
                    property: property.to_string(),
                    line,
                    context: context.to_string(),
                });
            }
            offset += 2 + end + 1;
            continue;
        }
        offset += rest.chars().next().map_or(1, char::len_utf8);
    }
}

fn validate_placeholder(
    reference: &PlaceholderRef,
    services: &IndexMap<String, Service>,
) -> Result<()> {
    let Some(service) = services.get(&reference.service) else {
        bail!(
            "unknown service '{}' referenced from {} at line {} (known services: {})",
            reference.service,
            reference.context,
            reference.line,
            services.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };

    match reference.property.as_str() {
        "host" => Ok(()),
        "port" if service.ports.len() == 1 => Ok(()),
        "port" if service.ports.is_empty() => bail!(
            "service '{}' exposes no ports, so '${{{}.port}}' in {} at line {} cannot resolve",
            reference.service,
            reference.service,
            reference.context,
            reference.line
        ),
        "port" => bail!(
            "service '{}' exposes multiple ports, so '${{{}.port}}' in {} at line {} is ambiguous; use one of: {}",
            reference.service,
            reference.service,
            reference.context,
            reference.line,
            service
                .ports
                .iter()
                .map(PortMapping::runtime_name)
                .map(|name| format!("${{{}.port.{}}}", reference.service, name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        property if property.starts_with("port.") => {
            let name = &property[5..];
            if service.ports.iter().any(|port| port.runtime_name() == name) {
                Ok(())
            } else {
                let known = service
                    .ports
                    .iter()
                    .map(PortMapping::runtime_name)
                    .collect::<Vec<_>>();
                let suffix = if known.is_empty() {
                    "this service has no named ports".to_string()
                } else {
                    format!("known named ports: {}", known.join(", "))
                };
                bail!(
                    "unknown port '{}' on service '{}' referenced from {} at line {} ({})",
                    name,
                    reference.service,
                    reference.context,
                    reference.line,
                    suffix
                )
            }
        }
        property => bail!(
            "unknown interpolation property '{}' on service '{}' referenced from {} at line {}; expected host, port, or port.<name>",
            property,
            reference.service,
            reference.context,
            reference.line
        ),
    }
}

/// Parse a port value as either `auto` (`None`) or a non-zero fixed port.
fn parse_port_value(value: &str, line_num: usize) -> Result<Option<NonZeroU16>> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let port: u16 = value
        .parse()
        .with_context(|| format!("invalid port number at line {}", line_num))?;
    NonZeroU16::new(port).map(Some).with_context(|| {
        format!(
            "invalid port number at line {}: port must be non-zero",
            line_num
        )
    })
}

fn parse_compose_expose(value: &str, alias: &str, line_num: usize) -> Result<(String, NonZeroU16)> {
    let (service, port) = value
        .split_once(':')
        .map_or((alias, value), |(service, port)| {
            (service.trim(), port.trim())
        });
    if !is_valid_compose_service_name(service) {
        bail!(
            "invalid Compose service name '{}' in expose.{} at line {}",
            service,
            alias,
            line_num
        );
    }
    let port: u16 = port
        .parse()
        .with_context(|| format!("invalid port number at line {}", line_num))?;
    let port = NonZeroU16::new(port).with_context(|| {
        format!(
            "invalid port number at line {}: port must be non-zero",
            line_num
        )
    })?;
    Ok((service.to_string(), port))
}

/// Set a single-valued property, rejecting a second occurrence. The old parser
/// silently let a later `image=`/`healthcheck=`/etc. overwrite an earlier one
/// (while list-valued properties accumulated), which made a duplicated line a
/// silent no-op; now it is an error.
fn set_once<T>(
    slot: &mut Option<T>,
    value: T,
    key: &str,
    service: &str,
    line_num: usize,
) -> Result<()> {
    if slot.is_some() {
        bail!(
            "duplicate '{}' in service '{}' at line {}; it was already set earlier \
             in the section",
            key,
            service,
            line_num
        );
    }
    *slot = Some(value);
    Ok(())
}

/// Set a service's source, rejecting a second one whatever its spelling:
/// `image=` twice, or `image=` plus `run=`, both leave the file's intent
/// ambiguous.
fn set_source(service: &mut ServiceBuilder, source: ServiceSource, line_num: usize) -> Result<()> {
    if service.source.is_some() {
        bail!(
            "service '{}' declares more than one source at line {}; set exactly \
             one of image/dockerfile/compose/run",
            service.name,
            line_num
        );
    }
    service.source = Some(source);
    Ok(())
}

fn parse_service_property(
    service: &mut ServiceBuilder,
    key: &str,
    value: &str,
    line_num: usize,
    refs: &mut Vec<PlaceholderRef>,
) -> Result<()> {
    // Most properties are meaningless with an empty value, and an empty value
    // usually means a templating or editing accident, so reject it up front.
    // `env.<KEY>=` stays legal: setting a variable to the empty string is
    // a real thing to want.
    let needs_value = matches!(
        key,
        "image"
            | "dockerfile"
            | "compose"
            | "run"
            | "role"
            | "command"
            | "volume"
            | "pre-start"
            | "post-start"
            | "pre-stop"
            | "post-stop"
            | "healthcheck"
            | "context"
    );
    if needs_value && value.is_empty() {
        bail!(
            "empty value for '{}' in service '{}' at line {}",
            key,
            service.name,
            line_num
        );
    }

    match key {
        "image" => set_source(service, ServiceSource::Image(value.to_string()), line_num)?,
        "dockerfile" => set_source(
            service,
            ServiceSource::Dockerfile(value.to_string()),
            line_num,
        )?,
        "compose" => set_source(service, ServiceSource::Compose(value.to_string()), line_num)?,
        // Shell command to run (non-Docker)
        "run" => set_source(service, ServiceSource::Command(value.to_string()), line_num)?,
        // The role this service belongs to (see `Service::role`).
        "role" => {
            let v = value.to_string();
            let name = service.name.clone();
            set_once(&mut service.role, v, key, &name, line_num)?;
        }
        // Container command override (for use with image/dockerfile)
        "command" => {
            let v = CommandOverride::parse(value, &service.name)?;
            set_once(
                &mut service.command_override,
                v,
                key,
                &service.name,
                line_num,
            )?;
        }
        "port" => {
            if service.has_port_named(None) {
                bail!(
                    "duplicate 'port' in service '{}' at line {}; name additional \
                     ports with port.<name>=",
                    service.name,
                    line_num
                );
            }
            let mapping = match parse_port_value(value, line_num)? {
                Some(port) => PortMapping::Fixed { name: None, port },
                None => PortMapping::Auto { name: None },
            };
            service.ports.push(mapping);
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
            scan_command_placeholders(
                value,
                line_num,
                &format!("healthcheck of service '{}'", service.name),
                refs,
            );
            let v = value.to_string();
            set_once(&mut service.healthcheck, v, key, &service.name, line_num)?;
        }
        "ready-timeout" => {
            let secs: u64 = value
                .parse()
                .with_context(|| format!("invalid timeout at line {}", line_num))?;
            let secs = NonZeroU64::new(secs).with_context(|| {
                format!(
                    "invalid timeout at line {}: ready-timeout must be non-zero",
                    line_num
                )
            })?;
            set_once(
                &mut service.ready_timeout_secs,
                secs,
                key,
                &service.name,
                line_num,
            )?;
        }
        key if key.starts_with("port.") => {
            let port_name = &key[5..];
            validate_port_name(port_name, "port", &service.name, line_num)?;
            if service.has_port_named(Some(port_name)) {
                bail!(
                    "duplicate 'port.{}' in service '{}' at line {}",
                    port_name,
                    service.name,
                    line_num
                );
            }
            let name = Some(port_name.to_string());
            let mapping = match parse_port_value(value, line_num)? {
                Some(port) => PortMapping::Fixed { name, port },
                None => PortMapping::Auto { name },
            };
            service.ports.push(mapping);
        }
        key if key.starts_with("env.") => {
            let env_name = &key[4..];
            if !is_valid_env_name(env_name) {
                bail!(
                    "invalid environment variable name '{}' in 'env.{}' of service \
                     '{}' at line {}",
                    env_name,
                    env_name,
                    service.name,
                    line_num
                );
            }
            reject_reserved_env_name(
                env_name,
                line_num,
                &format!("service '{}' environment", service.name),
            )?;
            if service.env.contains_key(env_name) {
                bail!(
                    "duplicate 'env.{}' in service '{}' at line {}",
                    env_name,
                    service.name,
                    line_num
                );
            }
            scan_placeholders(
                value,
                line_num,
                &format!("env.{} of service '{}'", env_name, service.name),
                refs,
            )?;
            service.env.insert(env_name.to_string(), value.to_string());
        }
        // For compose-based services, expose maps service ports
        key if key.starts_with("expose.") => {
            let port_name = &key[7..];
            validate_port_name(port_name, "expose", &service.name, line_num)?;
            if service.has_port_named(Some(port_name)) {
                bail!(
                    "duplicate 'expose.{}' in service '{}' at line {}",
                    port_name,
                    service.name,
                    line_num
                );
            }
            let (compose_service, port) = parse_compose_expose(value, port_name, line_num)?;
            service.expose.push(PortMapping::Compose {
                alias: port_name.to_string(),
                service: compose_service,
                port,
            });
        }
        // Build context for Dockerfiles
        "context" => {
            let v = value.to_string();
            set_once(&mut service.build_context, v, key, &service.name, line_num)?;
        }
        "roles_order" => {
            bail!(
                "'roles_order' at line {} must be declared at top level, above the \
                 first section, not inside service '{}'",
                line_num,
                service.name
            );
        }
        _ => {
            // The old parser reclassified an unknown SCREAMING_SNAKE_CASE key as
            // a trailing top-level variable, silently ending the section and
            // swallowing everything after it. That guess is gone; instead, say
            // where each kind of thing belongs.
            if is_valid_env_name(key) && key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            {
                bail!(
                    "'{}' at line {} looks like an environment variable, but it is \
                     inside service '{}' (sections do not end at blank lines). To \
                     set it in the container, write env.{}=...; to export it from \
                     `eph env`, move it into an [env] section or above the first \
                     section",
                    key,
                    line_num,
                    service.name,
                    key
                );
            }
            // A lowercase unknown key is most likely a typo'd property, but if
            // it is also a legal variable name, mention the other reading.
            let env_hint = if is_valid_env_name(key) {
                "; if you meant a top-level environment variable, move it into an \
                 [env] section"
            } else {
                ""
            };
            bail!(
                "unknown service property '{}' at line {} (known properties: {}){}",
                key,
                line_num,
                KNOWN_PROPERTIES,
                env_hint
            );
        }
    }
    Ok(())
}

/// Validate a `port.<name>`/`expose.<name>` port name: same shape as a service
/// name (it becomes part of `${service.port.<name>}` and `EPH_<SVC>_PORT_<NAME>`),
/// and never empty (a bare `port.=` is always an editing accident).
fn validate_port_name(name: &str, kind: &str, service: &str, line_num: usize) -> Result<()> {
    if name.is_empty() {
        bail!(
            "empty port name in '{}.' of service '{}' at line {}; write {}.<name>=<port>",
            kind,
            service,
            line_num,
            kind
        );
    }
    if !is_valid_service_name(name) {
        bail!(
            "invalid port name '{}' in service '{}' at line {}: port names are \
             lowercase letters, digits, and hyphens, starting with a letter",
            name,
            service,
            line_num
        );
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
/// returns `None`, the original `${...}` text is left untouched so it can be
/// surfaced unresolved. The escape `$${` renders as a literal `${` without
/// being treated as a placeholder. Text outside placeholders is copied
/// verbatim. This is the resolver used to expand [`EnvVar`] values once
/// services are running.
///
/// The parser guarantees every placeholder in a parsed file is well-formed
/// (`${service.property}` with a closing brace), so the lenient handling here
/// of malformed input (copied through verbatim) only matters for strings that
/// did not come from [`parse`].
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
/// An unresolved reference is left intact, and `$${` escapes to a literal:
///
/// ```
/// use eph::parser::resolve_interpolations;
///
/// assert_eq!(resolve_interpolations("${db.port}", |_, _| None), "${db.port}");
/// assert_eq!(resolve_interpolations("$${db.port}", |_, _| None), "${db.port}");
/// ```
#[must_use]
pub fn resolve_interpolations<F>(input: &str, resolver: F) -> String
where
    F: Fn(&str, &str) -> Option<String>,
{
    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        if rest.starts_with("$${") {
            result.push_str("${");
            i += 3;
            continue;
        }
        if let Some(tail) = rest.strip_prefix("${") {
            if let Some(end) = tail.find('}') {
                let content = &tail[..end];
                match content.split_once('.') {
                    Some((service, property)) => match resolver(service, property) {
                        Some(value) => result.push_str(&value),
                        None => {
                            // Keep original if not resolved
                            result.push_str("${");
                            result.push_str(content);
                            result.push('}');
                        }
                    },
                    None => {
                        result.push_str("${");
                        result.push_str(content);
                        result.push('}');
                    }
                }
                i += 2 + end + 1;
                continue;
            }
            // No closing brace: copy the tail verbatim rather than inventing one.
            result.push_str(rest);
            break;
        }
        let c = rest.chars().next().expect("i < input.len()");
        result.push(c);
        i += c.len_utf8();
    }
    result
}

/// Like [`resolve_interpolations`], but also returns every `${service.property}`
/// reference `resolver` could not resolve, as `(service, property)` pairs in
/// the order they appear. The `$${` escape is never a reference (it is
/// consumed by [`resolve_interpolations`] before `resolver` is ever called for
/// it), so it never appears in the returned list.
///
/// This is `eph env`'s hook into resolution: unlike every other `${...}`
/// consumer (lifecycle hooks, `eph run`, a service's own `env.` entries), which
/// all leave an unresolved reference verbatim so a hook that runs before its
/// referenced service starts still gets a placeholder it can recognize, `eph
/// env`'s output is meant to be handed straight to `eval`. A literal `${...}`
/// there would break the caller's shell, so its caller needs to know which
/// variables to omit instead.
///
/// # Examples
///
/// ```
/// use eph::parser::resolve_interpolations_tracked;
///
/// let (out, unresolved) = resolve_interpolations_tracked("${db.port}", |_, _| None);
/// assert_eq!(out, "${db.port}");
/// assert_eq!(unresolved, vec![("db".to_string(), "port".to_string())]);
///
/// // The `$${` escape is not a reference, so it is never reported.
/// let (out, unresolved) = resolve_interpolations_tracked("$${db.port}", |_, _| None);
/// assert_eq!(out, "${db.port}");
/// assert!(unresolved.is_empty());
/// ```
#[must_use]
pub fn resolve_interpolations_tracked<F>(
    input: &str,
    resolver: F,
) -> (String, Vec<(String, String)>)
where
    F: Fn(&str, &str) -> Option<String>,
{
    // `resolve_interpolations` requires a plain `Fn`, so interior mutability
    // (rather than a `FnMut` capture) is what lets this wrapping closure record
    // a miss without duplicating the placeholder-scanning logic above.
    let unresolved = std::cell::RefCell::new(Vec::new());
    let result = resolve_interpolations(input, |service, property| {
        let value = resolver(service, property);
        if value.is_none() {
            unresolved
                .borrow_mut()
                .push((service.to_string(), property.to_string()));
        }
        value
    });
    (result, unresolved.into_inner())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // Basics
    // ------------------------------------------------------------------------

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
        assert_eq!(pg.ports[0].container_port(), Some(5432));
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
    fn a_utf8_bom_is_ignored() {
        // Windows editors prepend a BOM; it must not end up in the first
        // variable's name (where it would break the emitted `export` line).
        let result = parse("\u{feff}APP=one\nB=two\n").unwrap();
        assert_eq!(result.env_vars[0].name, "APP");
        assert_eq!(result.env_vars[1].name, "B");
    }

    // ------------------------------------------------------------------------
    // The [env] section and variable placement
    // ------------------------------------------------------------------------

    #[test]
    fn env_section_declares_trailing_variables() {
        let input = r#"
[postgres]
image=postgres:16
port=5432

[env]
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/app
"#;
        let result = parse(input).unwrap();
        assert_eq!(result.env_vars.len(), 1);
        assert_eq!(result.env_vars[0].name, "DATABASE_URL");
        // Interpolation placeholders are stored verbatim; resolution is later.
        assert_eq!(
            result.env_vars[0].value,
            "postgres://dev:dev@localhost:${postgres.port}/app"
        );
    }

    #[test]
    fn env_section_may_repeat_and_order_is_preserved() {
        let input = r#"
FIRST=1

[redis]
image=redis:7
port=6379

[env]
SECOND=redis://localhost:${redis.port}

[web]
run=serve
port=auto

[env]
THIRD=3
"#;
        let result = parse(input).unwrap();
        let names: Vec<&str> = result.env_vars.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["FIRST", "SECOND", "THIRD"]);
    }

    #[test]
    fn bare_variable_after_a_section_is_rejected_with_guidance() {
        // The old parser silently ended the section here (and silently
        // reinterpreted every following line). Now it is a hard error that
        // says where the variable belongs.
        let input = "[postgres]\nimage=postgres:16\n\nDATABASE_URL=postgres://x\n";
        let err = parse(input).expect_err("bare trailing variable must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("DATABASE_URL"), "got: {msg}");
        assert!(msg.contains("[env]"), "got: {msg}");
        assert!(msg.contains("env.DATABASE_URL"), "got: {msg}");
    }

    #[test]
    fn misspelled_env_property_inside_section_is_rejected() {
        // The classic trap: POSTGRES_PASSWORD instead of env.POSTGRES_PASSWORD.
        // The old parser silently made it a shell variable and detached the rest
        // of the section; now the error names both possible intents.
        let input = "[postgres]\nimage=postgres:16\nPOSTGRES_PASSWORD=dev\nport=5432\n";
        let err = parse(input).expect_err("uppercase key inside a section must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("env.POSTGRES_PASSWORD"), "got: {msg}");
        assert!(msg.contains("[env]"), "got: {msg}");
    }

    #[test]
    fn unknown_lowercase_key_in_section_lists_known_properties() {
        let input = "[postgres]\nimage=postgres:16\nprot=5432\n";
        let err = parse(input).expect_err("typo'd property must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown service property 'prot'"),
            "got: {msg}"
        );
        assert!(msg.contains("healthcheck"), "got: {msg}");
    }

    #[test]
    fn duplicate_top_level_variable_is_rejected() {
        let err = parse("FOO=first\nFOO=second\n").expect_err("duplicate var must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate environment variable 'FOO'"),
            "got: {msg}"
        );
    }

    #[test]
    fn duplicate_across_top_level_and_env_section_is_rejected() {
        // The top-of-file block and [env] sections share one namespace.
        let input = "FOO=first\n\n[web]\nrun=serve\n\n[env]\nFOO=second\n";
        assert!(parse(input).is_err());
    }

    #[test]
    fn invalid_variable_name_is_rejected() {
        // `export 1BAD=x` would fail at eval time; catch it at parse time.
        let err = parse("1BAD=x\n").expect_err("invalid identifier must be rejected");
        assert!(err.to_string().contains("1BAD"), "got: {err}");
    }

    #[test]
    fn lowercase_variable_names_are_allowed_at_top_level() {
        // Names only need to be shell-legal; case is the author's business.
        let result = parse("flask_debug=1\n").unwrap();
        assert_eq!(result.env_vars[0].name, "flask_debug");
    }

    #[test]
    fn eph_metadata_names_are_reserved_in_every_environment_scope() {
        for input in [
            "EPH_WORKSPACE_ID=shadowed\n",
            "eph_workspace_id=shadowed\n",
            "[web]\nrun=serve\nenv.EPH_WEB_PORT=shadowed\n",
        ] {
            let error = parse(input).expect_err("EPH_* must remain owned by eph");
            assert!(error.to_string().contains("managed by eph"), "got: {error}");
        }
    }

    #[test]
    fn roles_order_inside_env_section_is_rejected() {
        let input = "[db]\nimage=postgres:16\nrole=dep\n\n[env]\nroles_order=dep\n";
        let err = parse(input).expect_err("roles_order inside [env] must be rejected");
        assert!(err.to_string().contains("reserved"), "got: {err}");
    }

    // ------------------------------------------------------------------------
    // Section names and duplicates
    // ------------------------------------------------------------------------

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
    fn duplicate_section_is_rejected() {
        // The old parser silently merged reopened sections (with later scalar
        // keys overwriting earlier ones, so [db] could end up running mysql
        // with postgres's port). Now a reopened section is an error.
        let input = "[db]\nimage=postgres:16\nport=5432\n\n[db]\nimage=mysql:8\n";
        let err = parse(input).expect_err("duplicate section must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("duplicate section [db]"), "got: {msg}");
        assert!(msg.contains("first defined at line 1"), "got: {msg}");
    }

    #[test]
    fn invalid_service_names_are_rejected() {
        // Names leak into container names, ${name.prop} interpolation (dots
        // would split wrong), and EPH_<NAME>_* variables (where `-` and `_`
        // must not collide), so the character set is strict.
        for name in ["My-Service", "my db", "db.primary", "auth_db", "-db", "1db"] {
            let input = format!("[{name}]\nimage=busybox\n");
            assert!(parse(&input).is_err(), "[{name}] must be rejected");
        }
    }

    #[test]
    fn misspelled_reserved_sections_get_a_hint() {
        let err = parse("[db]\nimage=postgres:16\nrole=dep\n[role_order]\ndep=\n")
            .expect_err("[role_order] must be rejected");
        assert!(err.to_string().contains("[roles_order]"), "got: {err}");

        let err = parse("[web]\nrun=serve\n[vars]\nFOO=1\n").expect_err("[vars] must be rejected");
        assert!(err.to_string().contains("[env]"), "got: {err}");
    }

    // ------------------------------------------------------------------------
    // Property values and duplicates
    // ------------------------------------------------------------------------

    #[test]
    fn duplicate_source_is_rejected_even_across_spellings() {
        assert!(parse("[db]\nimage=postgres:16\nimage=mysql:8\n").is_err());
        let err = parse("[db]\nimage=postgres:16\nrun=serve\n")
            .expect_err("two sources must be rejected");
        assert!(
            err.to_string().contains("more than one source"),
            "got: {err}"
        );
    }

    #[test]
    fn duplicate_scalar_properties_are_rejected() {
        assert!(parse("[db]\nimage=postgres:16\nhealthcheck=a\nhealthcheck=b\n").is_err());
        assert!(parse("[db]\nimage=postgres:16\nready-timeout=5\nready-timeout=9\n").is_err());
        assert!(parse("[db]\nimage=postgres:16\ncommand=a\ncommand=b\n").is_err());
    }

    #[test]
    fn duplicate_ports_and_env_keys_are_rejected() {
        assert!(parse("[web]\nrun=serve\nport=3000\nport=4000\n").is_err());
        assert!(parse("[web]\nrun=serve\nport.api=3000\nport.api=4000\n").is_err());
        assert!(parse("[web]\nrun=serve\nport=3000\nport.default=4000\n").is_err());
        assert!(parse("[db]\nimage=postgres:16\nenv.X=1\nenv.X=2\n").is_err());
    }

    #[test]
    fn empty_values_are_rejected_for_most_properties() {
        assert!(parse("[db]\nimage=\n").is_err());
        assert!(parse("[db]\nimage=postgres:16\nhealthcheck=\n").is_err());
        assert!(parse("[db]\nimage=postgres:16\nvolume=\n").is_err());
        assert!(parse("[db]\nimage=postgres:16\npost-start=\n").is_err());
        // Setting a container variable to the empty string stays legal.
        let eph = parse("[db]\nimage=postgres:16\nenv.EMPTY=\n").unwrap();
        assert_eq!(eph.services["db"].env.get("EMPTY"), Some(&String::new()));
    }

    #[test]
    fn empty_and_invalid_dotted_names_are_rejected() {
        assert!(parse("[web]\nrun=serve\nport.=8080\n").is_err());
        assert!(parse("[web]\nrun=serve\nenv.=x\n").is_err());
        assert!(parse("[web]\nrun=serve\nport.API=8080\n").is_err());
        assert!(parse("[web]\nrun=serve\nenv.1BAD=x\n").is_err());
        assert!(parse("[stack]\ncompose=dc.yml\nexpose.=9000\n").is_err());
    }

    #[test]
    fn command_is_only_legal_for_image_and_dockerfile_services() {
        // On run=/compose= services a command= was silently ignored; now it
        // is rejected where it can do nothing.
        assert!(parse("[m]\nimage=minio/minio\ncommand=server /data\n").is_ok());
        let err = parse("[w]\nrun=serve\ncommand=other\n").expect_err("command on run=");
        assert!(err.to_string().contains("command"), "got: {err}");
        assert!(parse("[s]\ncompose=dc.yml\ncommand=other\n").is_err());
    }

    #[test]
    fn command_override_is_parsed_into_argv_at_the_file_boundary() {
        let eph = parse("[web]\nimage=example/web\ncommand=sh -c \"echo hi\"\n").unwrap();
        assert_eq!(
            eph.services["web"]
                .command_override
                .as_ref()
                .unwrap()
                .argv(),
            ["sh", "-c", "echo hi"]
        );

        let error = parse("[web]\nimage=example/web\ncommand=sh -c \"echo hi\n")
            .expect_err("unbalanced command quoting must fail during parse");
        assert!(
            error
                .to_string()
                .contains("invalid command override for service 'web'"),
            "got: {error}"
        );
    }

    #[test]
    fn source_specific_properties_are_rejected_when_the_backend_ignores_them() {
        assert!(parse("[web]\nrun=serve\ncontext=.\n").is_err());
        assert!(parse("[web]\nimage=example/web\ncontext=.\n").is_err());
        assert!(parse("[web]\nrun=serve\nvolume=data:/data\n").is_err());
        assert!(parse("[stack]\ncompose=compose.yml\nvolume=data:/data\n").is_err());
        assert!(parse("[web]\ndockerfile=Dockerfile\ncontext=.\nvolume=data:/data\n").is_ok());
        assert!(parse("[web]\nimage=example/web\nvolume=data:/data\n").is_ok());
    }

    #[test]
    fn readiness_timeout_requires_a_healthcheck_and_must_be_non_zero() {
        assert!(parse("[web]\nrun=serve\nready-timeout=5\n").is_err());
        assert!(parse("[web]\nrun=serve\nhealthcheck=true\nready-timeout=0\n").is_err());

        let eph = parse("[web]\nrun=serve\nhealthcheck=true\nready-timeout=5\n").unwrap();
        assert_eq!(
            eph.services["web"]
                .healthcheck
                .as_ref()
                .unwrap()
                .timeout_secs
                .unwrap()
                .get(),
            5
        );
    }

    #[test]
    fn expose_and_port_are_tied_to_their_backends() {
        // expose.<name>= names compose-project ports; port= is for services eph
        // publishes itself. Each is rejected on the other backend.
        assert!(parse("[stack]\ncompose=dc.yml\nexpose.api=9000\n").is_ok());
        let err =
            parse("[db]\nimage=postgres:16\nexpose.api=9000\n").expect_err("expose on image=");
        assert!(err.to_string().contains("compose"), "got: {err}");
        let err = parse("[stack]\ncompose=dc.yml\nport=9000\n").expect_err("port on compose=");
        assert!(err.to_string().contains("expose"), "got: {err}");
    }

    #[test]
    fn compose_expose_carries_alias_and_target_service_separately() {
        let eph =
            parse("[stack]\ncompose=compose.yml\nexpose.cache=redis-main:6379\nexpose.api=8080\n")
                .unwrap();
        let stack = &eph.services["stack"];

        assert!(matches!(
            &stack.ports[0],
            PortMapping::Compose { alias, service, port }
                if alias == "cache" && service == "redis-main" && port.get() == 6379
        ));
        assert!(matches!(
            &stack.ports[1],
            PortMapping::Compose { alias, service, port }
                if alias == "api" && service == "api" && port.get() == 8080
        ));
    }

    // ------------------------------------------------------------------------
    // Ports
    // ------------------------------------------------------------------------

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

        let unnamed = web.ports.iter().find(|p| p.name().is_none()).unwrap();
        assert!(unnamed.is_auto());
        assert_eq!(unnamed.container_port(), None);

        let hmr = web.ports.iter().find(|p| p.name() == Some("hmr")).unwrap();
        assert!(hmr.is_auto());

        // A fixed numeric port alongside auto ports stays fixed.
        let api = web.ports.iter().find(|p| p.name() == Some("api")).unwrap();
        assert!(!api.is_auto());
        assert_eq!(api.container_port(), Some(5000));
    }

    #[test]
    fn test_auto_port_is_case_insensitive() {
        let result = parse("[web]\nrun=serve\nport=AUTO\n").unwrap();
        assert!(result.services["web"].ports[0].is_auto());
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
    fn zero_fixed_and_compose_ports_are_rejected() {
        assert!(parse("[web]\nrun=serve\nport=0\n").is_err());
        assert!(parse("[web]\nimage=example/web\nport.api=0\n").is_err());
        assert!(parse("[stack]\ncompose=compose.yml\nexpose.api=0\n").is_err());
        assert!(parse("[stack]\ncompose=compose.yml\nexpose.cache=redis:0\n").is_err());
    }

    // ------------------------------------------------------------------------
    // Interpolation
    // ------------------------------------------------------------------------

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
        // A well-formed reference whose resolver declines stays verbatim, so the
        // unresolved reference is visible downstream.
        let input = "redis://localhost:${redis.port}/0";
        let result = resolve_interpolations(input, |_service, _property| None);
        assert_eq!(result, "redis://localhost:${redis.port}/0");
    }

    #[test]
    fn resolve_interpolations_honors_the_escape() {
        let result = resolve_interpolations("cost: $${redis.port}", |_s, _p| {
            Some("should not be used".to_string())
        });
        assert_eq!(result, "cost: ${redis.port}");
    }

    #[test]
    fn resolve_interpolations_copies_an_unterminated_placeholder_verbatim() {
        // The old implementation invented a closing brace; the tail must now
        // pass through untouched. (parse() rejects this shape anyway; this
        // matters for strings that did not come from the parser.)
        let result = resolve_interpolations("x ${db.port", |_s, _p| Some("5432".to_string()));
        assert_eq!(result, "x ${db.port");
    }

    #[test]
    fn unterminated_placeholder_is_a_parse_error() {
        let input = "[db]\nimage=postgres:16\n\n[env]\nURL=postgres://x:${db.port\n";
        let err = parse(input).expect_err("unterminated ${ must be rejected");
        assert!(err.to_string().contains("unterminated"), "got: {err}");
    }

    #[test]
    fn dotless_placeholder_is_a_parse_error_with_escape_hint() {
        let input = "[env]\nGREETING=${name}\n";
        let err = parse(input).expect_err("dotless ${} must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("service.property"), "got: {msg}");
        assert!(msg.contains("$${"), "got: {msg}");
    }

    #[test]
    fn escaped_placeholder_parses_and_needs_no_service() {
        // $${ is a literal; it must not be validated as a reference.
        let eph = parse("TEMPLATE=$${not.a.service}\n").unwrap();
        assert_eq!(eph.env_vars[0].value, "$${not.a.service}");
    }

    #[test]
    fn unknown_service_reference_is_a_parse_error() {
        let input = "[db]\nimage=postgres:16\n\n[env]\nURL=${bd.port}\n";
        let err = parse(input).expect_err("unknown service reference must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("unknown service 'bd'"), "got: {msg}");
        assert!(msg.contains("db"), "got: {msg}");
    }

    #[test]
    fn forward_reference_to_a_later_service_is_fine() {
        let input = "URL=${db.port}\n\n[db]\nimage=postgres:16\nport=5432\n";
        assert!(parse(input).is_ok());
    }

    #[test]
    fn service_env_references_are_validated_too() {
        let input = "[web]\nrun=serve\nport=auto\nenv.PORT=${wbe.port}\n";
        let err = parse(input).expect_err("unknown service in env.* must be rejected");
        assert!(err.to_string().contains("wbe"), "got: {err}");
    }

    #[test]
    fn interpolation_properties_must_exist_and_bare_port_must_be_unambiguous() {
        let cases = [
            (
                "[db]\nimage=postgres:16\n\n[env]\nURL=${db.typo}\n",
                "unknown interpolation property 'typo'",
            ),
            (
                "[db]\nimage=postgres:16\nport.sql=5432\n\n[env]\nURL=${db.port.admin}\n",
                "unknown port 'admin'",
            ),
            (
                "[db]\nimage=postgres:16\n\n[env]\nURL=${db.port}\n",
                "exposes no ports",
            ),
            (
                "[db]\nimage=postgres:16\nport.sql=5432\nport.admin=5433\n\n[env]\nURL=${db.port}\n",
                "is ambiguous",
            ),
        ];
        for (input, expected) in cases {
            let error = parse(input).expect_err("invalid interpolation must fail during parse");
            assert!(error.to_string().contains(expected), "got: {error}");
        }

        assert!(parse("[db]\nimage=postgres:16\nport.sql=5432\n\n[env]\nURL=${db.port}\n").is_ok());
        assert!(
            parse(
                "[web]\nrun=serve\nport=3000\nport.admin=3001\n\n[env]\nURL=${web.port.default}\n"
            )
            .is_ok()
        );
    }

    #[test]
    fn shell_commands_preserve_shell_expansion_while_healthchecks_check_eph_references() {
        // Dotless shell variables belong to the shell. Dotted healthcheck
        // placeholders belong to eph and receive the same semantic checks as
        // environment values.
        let input =
            "[web]\nrun=echo ${PORT}\nhealthcheck=test -n ${HOME}\npost-start=echo ${undefined}\n";
        assert!(parse(input).is_ok());

        let input = "[web]\nrun=serve\nport=3000\nhealthcheck=test ${web.port.missing}\n";
        assert!(parse(input).is_err());

        let input = "[web]\nrun=serve\nport=3000\nhealthcheck=echo $${ghost.port}\n";
        assert!(parse(input).is_ok());
    }

    // ------------------------------------------------------------------------
    // Quotes
    // ------------------------------------------------------------------------

    #[test]
    fn strip_quotes_removes_matching_double_quotes() {
        assert_eq!(strip_quotes("\"hello\""), "hello");
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
    fn strip_quotes_does_not_mangle_interior_quotes() {
        // `"a" and "b"` used to become `a" and "b`; if the outer pair is not
        // unambiguous the value passes through whole.
        assert_eq!(strip_quotes("\"a\" and \"b\""), "\"a\" and \"b\"");
        assert_eq!(strip_quotes("\"it's\""), "it's");
    }

    #[test]
    fn strip_quotes_survives_a_single_quote_character() {
        // A value that is exactly one quote character used to panic the parser.
        assert_eq!(strip_quotes("\""), "\"");
        assert_eq!(strip_quotes("'"), "'");
        let eph = parse("FOO=\"\n").unwrap();
        assert_eq!(eph.env_vars[0].value, "\"");
    }

    // ------------------------------------------------------------------------
    // Name helpers
    // ------------------------------------------------------------------------

    #[test]
    fn service_name_rules() {
        assert!(is_valid_service_name("db"));
        assert!(is_valid_service_name("auth-db2"));
        assert!(!is_valid_service_name(""));
        assert!(!is_valid_service_name("Db"));
        assert!(!is_valid_service_name("auth_db"));
        assert!(!is_valid_service_name("2db"));
        assert!(!is_valid_service_name("-db"));
        assert!(!is_valid_service_name("a.b"));
    }

    #[test]
    fn env_name_rules() {
        assert!(is_valid_env_name("DATABASE_URL"));
        assert!(is_valid_env_name("_private"));
        assert!(is_valid_env_name("flask_debug"));
        assert!(!is_valid_env_name(""));
        assert!(!is_valid_env_name("1BAD"));
        assert!(!is_valid_env_name("foo-bar"));
        assert!(!is_valid_env_name("a.b"));
    }

    // ========================================================================
    // Roles and roles_order
    // ========================================================================

    #[test]
    fn parse_service_role() {
        let eph = parse("roles_order=dep\n\n[postgres]\nimage=postgres:16\nrole=dep\n").unwrap();
        assert_eq!(eph.services["postgres"].role.as_deref(), Some("dep"));
    }

    #[test]
    fn no_roles_is_legacy_mode() {
        // A file with no role= and no roles_order stays in legacy mode: no graph,
        // and start order is declaration order with run= services last.
        let eph = parse("[postgres]\nimage=postgres:16\n\n[web]\nrun=serve\n").unwrap();
        assert!(eph.roles_order.is_none());
        assert!(eph.services["postgres"].role.is_none());
        let order: Vec<&str> = eph.start_order().iter().map(|s| s.as_str()).collect();
        assert_eq!(order, ["postgres", "web"]);
    }

    #[test]
    fn linear_roles_order_desugars_to_a_chain() {
        let eph = parse(
            "roles_order=dep,app\n\n[db]\nimage=postgres:16\nrole=dep\n\n[web]\nrun=serve\nrole=app\n",
        )
        .unwrap();
        let order = eph.roles_order.as_ref().unwrap();
        assert_eq!(order.deps["dep"], Vec::<String>::new());
        assert_eq!(order.deps["app"], vec!["dep".to_string()]);
        assert_eq!(order.topo_roles().unwrap(), vec!["dep", "app"]);
    }

    #[test]
    fn dag_roles_order_section_parses_edges() {
        // worker depends on dep but NOT app, so it can come up without app.
        let eph = parse(
            "[db]\nimage=postgres:16\nrole=dep\n\
             [web]\nrun=serve\nrole=app\n\
             [jobs]\nrun=worker\nrole=worker\n\
             [roles_order]\ndep=\napp=dep\nworker=dep\n",
        )
        .unwrap();
        let order = eph.roles_order.as_ref().unwrap();
        assert_eq!(order.deps["dep"], Vec::<String>::new());
        assert_eq!(order.deps["app"], vec!["dep".to_string()]);
        assert_eq!(order.deps["worker"], vec!["dep".to_string()]);
        // dep sorts first; app and worker both follow it, breaking the tie by
        // declaration order in the section (app before worker).
        assert_eq!(order.topo_roles().unwrap(), vec!["dep", "app", "worker"]);
    }

    #[test]
    fn start_order_groups_by_role_in_topological_order() {
        // Services are declared out of role order; start_order regroups them by
        // the role graph, keeping declaration order within a role.
        let eph = parse(
            "roles_order=dep,app\n\
             [web]\nrun=serve\nrole=app\n\
             [db]\nimage=postgres:16\nrole=dep\n\
             [cache]\nimage=redis:7\nrole=dep\n",
        )
        .unwrap();
        let order: Vec<&str> = eph.start_order().iter().map(|s| s.as_str()).collect();
        // Both dep services (in declaration order db, cache) before the app.
        assert_eq!(order, ["db", "cache", "web"]);
    }

    #[test]
    fn forward_closure_pulls_in_dependencies_only() {
        let eph = parse(
            "[db]\nimage=postgres:16\nrole=dep\n\
             [web]\nrun=serve\nrole=app\n\
             [jobs]\nrun=worker\nrole=worker\n\
             [roles_order]\ndep=\napp=dep\nworker=dep\n",
        )
        .unwrap();
        // --role=worker brings up worker + dep, but NOT app.
        assert_eq!(
            eph.services_for_roles_up(&["worker".to_string()]).unwrap(),
            vec!["db".to_string(), "jobs".to_string()]
        );
        // --role=app brings up app + dep, but NOT worker.
        assert_eq!(
            eph.services_for_roles_up(&["app".to_string()]).unwrap(),
            vec!["db".to_string(), "web".to_string()]
        );
    }

    #[test]
    fn reverse_closure_pulls_in_dependents() {
        let eph = parse(
            "[db]\nimage=postgres:16\nrole=dep\n\
             [web]\nrun=serve\nrole=app\n\
             [jobs]\nrun=worker\nrole=worker\n\
             [roles_order]\ndep=\napp=dep\nworker=dep\n",
        )
        .unwrap();
        // Tearing down dep must also take everything that depends on it, returned
        // in bring-up order (the caller stops in reverse).
        assert_eq!(
            eph.services_for_roles_down(&["dep".to_string()]).unwrap(),
            vec!["db".to_string(), "web".to_string(), "jobs".to_string()]
        );
    }

    #[test]
    fn role_flag_on_a_file_without_roles_is_an_error() {
        let eph = parse("[postgres]\nimage=postgres:16\n").unwrap();
        assert!(eph.services_for_roles_up(&["dep".to_string()]).is_err());
    }

    #[test]
    fn unknown_role_in_selection_is_an_error() {
        let eph = parse("roles_order=dep\n\n[db]\nimage=postgres:16\nrole=dep\n").unwrap();
        let err = eph
            .services_for_roles_up(&["nope".to_string()])
            .expect_err("unknown role must error");
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn tagging_a_role_without_roles_order_is_rejected() {
        let input = "[postgres]\nimage=postgres:16\nrole=dep\n";
        let err = parse(input).expect_err("role without roles_order must be rejected");
        assert!(err.to_string().contains("roles_order"));
    }

    #[test]
    fn roles_order_with_an_untagged_service_is_rejected() {
        let input = "roles_order=dep\n\n[db]\nimage=postgres:16\nrole=dep\n\n[web]\nrun=serve\n";
        let err = parse(input).expect_err("untagged service under roles_order must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("web") && msg.contains("role"));
    }

    #[test]
    fn service_role_not_in_roles_order_is_rejected() {
        let input = "roles_order=dep\n\n[web]\nrun=serve\nrole=app\n";
        let err = parse(input).expect_err("service role missing from roles_order must be rejected");
        assert!(err.to_string().contains("app"));
    }

    #[test]
    fn roles_order_role_without_a_service_is_rejected() {
        // `cache` is listed in roles_order but no service declares it.
        let input = "roles_order=dep,cache\n\n[db]\nimage=postgres:16\nrole=dep\n";
        let err = parse(input).expect_err("role with no service must be rejected");
        assert!(err.to_string().contains("cache"));
    }

    #[test]
    fn dependency_on_unknown_role_is_rejected() {
        let input = "[db]\nimage=postgres:16\nrole=dep\n[roles_order]\ndep=ghost\n";
        let err = parse(input).expect_err("edge to unknown role must be rejected");
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn cyclic_roles_order_is_rejected() {
        let input = "[a]\nrun=a\nrole=x\n[b]\nrun=b\nrole=y\n[roles_order]\nx=y\ny=x\n";
        let err = parse(input).expect_err("a role cycle must be rejected");
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn declaring_both_roles_order_forms_is_rejected() {
        let input = "roles_order=dep\n\n[db]\nimage=postgres:16\nrole=dep\n\n[roles_order]\ndep=\n";
        let err = parse(input).expect_err("both linear and section forms must be rejected");
        assert!(err.to_string().contains("both"));
    }

    #[test]
    fn duplicate_role_key_in_dag_is_rejected() {
        let input = "[db]\nimage=postgres:16\nrole=dep\n[roles_order]\ndep=\ndep=\n";
        let err = parse(input).expect_err("a duplicate role key must be rejected");
        assert!(err.to_string().contains("duplicate role 'dep'"));
    }

    #[test]
    fn duplicate_role_in_linear_form_is_rejected() {
        let input = "roles_order=dep,dep\n\n[db]\nimage=postgres:16\nrole=dep\n";
        let err = parse(input).expect_err("a repeated role in the linear form must be rejected");
        assert!(err.to_string().contains("duplicate role 'dep'"));
    }

    #[test]
    fn empty_role_segments_are_rejected_in_both_order_forms() {
        for input in [
            "roles_order=dep,,app\n\n[db]\nrun=db\nrole=dep\n[web]\nrun=web\nrole=app\n",
            "roles_order=dep,app,\n\n[db]\nrun=db\nrole=dep\n[web]\nrun=web\nrole=app\n",
            "[db]\nrun=db\nrole=dep\n[web]\nrun=web\nrole=app\n[roles_order]\ndep=\napp=dep,,cache\n",
        ] {
            let error = parse(input).expect_err("empty role segments must not be discarded");
            assert!(error.to_string().contains("empty role"), "got: {error}");
        }
    }

    #[test]
    fn duplicate_dependencies_in_dag_form_are_rejected() {
        let input =
            "[db]\nrun=db\nrole=dep\n[web]\nrun=web\nrole=app\n[roles_order]\ndep=\napp=dep,dep\n";
        let error = parse(input).expect_err("duplicate dependency must be rejected");
        assert!(error.to_string().contains("duplicate role 'dep'"));
    }

    #[test]
    fn empty_role_name_in_dag_form_is_rejected() {
        let input = "[db]\nrun=db\nrole=dep\n[roles_order]\n=dep\ndep=\n";
        let error = parse(input).expect_err("an empty role key must be rejected");
        assert!(error.to_string().contains("empty role name"));
    }

    #[test]
    fn duplicate_role_property_in_a_service_is_rejected() {
        let input = "roles_order=dep\n\n[db]\nimage=postgres:16\nrole=dep\nrole=dep\n";
        let err = parse(input).expect_err("a duplicate role= line must be rejected");
        assert!(err.to_string().contains("duplicate 'role'"), "got: {err}");
    }

    #[test]
    fn role_names_are_free_form_including_uppercase() {
        // Role names are not restricted to any case: an uppercase role works in
        // both the section and linear forms.
        let eph = parse("[db]\nimage=postgres:16\nrole=DEP\n\n[roles_order]\nDEP=\n").unwrap();
        assert_eq!(eph.services["db"].role.as_deref(), Some("DEP"));
        assert!(eph.roles_order.as_ref().unwrap().deps.contains_key("DEP"));
    }

    #[test]
    fn roles_order_section_can_precede_the_services() {
        // The section may appear anywhere, including before the services it names.
        let eph = parse(
            "[roles_order]\ndep=\napp=dep\n\n[db]\nimage=postgres:16\nrole=dep\n\n[web]\nrun=serve\nrole=app\n",
        )
        .unwrap();
        let order: Vec<&str> = eph.start_order().iter().map(|s| s.as_str()).collect();
        assert_eq!(order, ["db", "web"]);
    }

    // ------------------------------------------------------------------------
    // resolve_interpolations_tracked
    // ------------------------------------------------------------------------

    #[test]
    fn tracked_resolve_reports_a_genuine_unresolved_reference() {
        let (out, unresolved) = resolve_interpolations_tracked("${db.port}", |_, _| None);
        assert_eq!(out, "${db.port}");
        assert_eq!(unresolved, vec![("db".to_string(), "port".to_string())]);
    }

    #[test]
    fn tracked_resolve_does_not_report_the_escaped_form() {
        // `$${` is a literal `${` and is never treated as a reference, so it
        // must never show up as unresolved.
        let (out, unresolved) = resolve_interpolations_tracked("$${db.port}", |_, _| None);
        assert_eq!(out, "${db.port}");
        assert!(unresolved.is_empty());
    }

    #[test]
    fn tracked_resolve_does_not_report_a_resolvable_reference() {
        let (out, unresolved) = resolve_interpolations_tracked("${db.port}", |svc, prop| {
            (svc == "db" && prop == "port").then(|| "5432".to_string())
        });
        assert_eq!(out, "5432");
        assert!(unresolved.is_empty());
    }

    #[test]
    fn tracked_resolve_reports_only_the_references_that_actually_missed() {
        let (out, unresolved) = resolve_interpolations_tracked(
            "${redis.host}:${redis.port}/${db.port}",
            |svc, prop| (svc == "redis").then(|| format!("{svc}-{prop}")),
        );
        assert_eq!(out, "redis-host:redis-port/${db.port}");
        assert_eq!(unresolved, vec![("db".to_string(), "port".to_string())]);
    }
}
