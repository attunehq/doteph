//! Lifecycle hook configuration and execution shared by workspace commands and prune.

use crate::parser::{EphFile, Service, ServiceSource, is_valid_env_name, is_valid_service_name};
use crate::service::{RunningService, UnresolvedEnvironment, resolve_env_pairs_strict};
use crate::{Workspace, proc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The backend family a hook-bearing service used when its teardown snapshot was saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CleanupKind {
    DirectContainer,
    Compose,
    Process,
}

/// Hook configuration retained after a workspace path disappears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeardownHookSnapshot {
    env_vars: Vec<HookEnvVar>,
    services: Vec<TeardownHookService>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HookEnvVar {
    name: String,
    value: String,
}

impl<'de> Deserialize<'de> for TeardownHookSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawSnapshot {
            env_vars: Vec<HookEnvVar>,
            services: Vec<TeardownHookService>,
        }

        let raw = RawSnapshot::deserialize(deserializer)?;
        if raw.services.is_empty() {
            return Err(serde::de::Error::custom(
                "teardown hook snapshot has no hook-bearing services",
            ));
        }

        let mut env_names = HashSet::new();
        for variable in &raw.env_vars {
            if !is_valid_env_name(&variable.name) {
                return Err(serde::de::Error::custom(format!(
                    "invalid top-level hook environment variable '{}'",
                    variable.name
                )));
            }
            if is_reserved_env_name(&variable.name) {
                return Err(serde::de::Error::custom(format!(
                    "reserved top-level EPH_* hook environment variable '{}'",
                    variable.name
                )));
            }
            if !env_names.insert(&variable.name) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate top-level hook environment variable '{}'",
                    variable.name
                )));
            }
        }

        let mut service_names = HashSet::new();
        for service in &raw.services {
            if !is_valid_service_name(&service.name) {
                return Err(serde::de::Error::custom(format!(
                    "invalid teardown hook service name '{}'",
                    service.name
                )));
            }
            if !service_names.insert(&service.name) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate teardown hook service '{}'",
                    service.name
                )));
            }
            if !service.has_hooks() {
                return Err(serde::de::Error::custom(format!(
                    "teardown hook service '{}' has no hooks",
                    service.name
                )));
            }
            if service.commands().any(|command| command.trim().is_empty()) {
                return Err(serde::de::Error::custom(format!(
                    "teardown hook service '{}' has an empty hook command",
                    service.name
                )));
            }
            for name in service.env.keys() {
                if !is_valid_env_name(name) {
                    return Err(serde::de::Error::custom(format!(
                        "invalid hook environment variable '{name}' for service '{}'",
                        service.name
                    )));
                }
                if is_reserved_env_name(name) {
                    return Err(serde::de::Error::custom(format!(
                        "reserved EPH_* hook environment variable '{name}' for service '{}'",
                        service.name
                    )));
                }
            }
        }

        Ok(Self {
            env_vars: raw.env_vars,
            services: raw.services,
        })
    }
}

fn is_reserved_env_name(name: &str) -> bool {
    name.get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("EPH_"))
}

/// One service's teardown hooks, stored in start order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeardownHookService {
    pub(crate) name: String,
    pub(crate) cleanup_kind: CleanupKind,
    env: HashMap<String, String>,
    pub(crate) pre_stop: Vec<String>,
    pub(crate) post_stop: Vec<String>,
    pub(crate) pre_clean: Vec<String>,
    pub(crate) post_clean: Vec<String>,
}

impl<'de> Deserialize<'de> for TeardownHookService {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawService {
            name: String,
            cleanup_kind: CleanupKind,
            env: StrictEnv,
            pre_stop: Vec<String>,
            post_stop: Vec<String>,
            pre_clean: Vec<String>,
            post_clean: Vec<String>,
        }

        let raw = RawService::deserialize(deserializer)?;
        Ok(Self {
            name: raw.name,
            cleanup_kind: raw.cleanup_kind,
            env: raw.env.0,
            pre_stop: raw.pre_stop,
            post_stop: raw.post_stop,
            pre_clean: raw.pre_clean,
            post_clean: raw.post_clean,
        })
    }
}

struct StrictEnv(HashMap<String, String>);

impl<'de> Deserialize<'de> for StrictEnv {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct EnvVisitor;

        impl<'de> serde::de::Visitor<'de> for EnvVisitor {
            type Value = StrictEnv;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an environment object with unique keys")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut env = HashMap::new();
                while let Some((name, value)) = map.next_entry::<String, String>()? {
                    if env.insert(name.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate hook environment variable '{name}'"
                        )));
                    }
                }
                Ok(StrictEnv(env))
            }
        }

        deserializer.deserialize_map(EnvVisitor)
    }
}

impl TeardownHookService {
    fn has_hooks(&self) -> bool {
        self.commands().next().is_some()
    }

    fn commands(&self) -> impl Iterator<Item = &String> {
        self.pre_stop
            .iter()
            .chain(&self.post_stop)
            .chain(&self.pre_clean)
            .chain(&self.post_clean)
    }
}

impl TeardownHookSnapshot {
    /// Capture the minimum configuration needed to run teardown hooks later.
    pub(crate) fn capture(eph: &EphFile) -> Option<Self> {
        let services = eph
            .start_order()
            .into_iter()
            .filter_map(|name| {
                let service = &eph.services[name];
                service.has_teardown_hooks().then(|| TeardownHookService {
                    name: name.clone(),
                    cleanup_kind: CleanupKind::from(&service.source),
                    env: service.env.clone(),
                    pre_stop: service.pre_stop.clone(),
                    post_stop: service.post_stop.clone(),
                    pre_clean: service.pre_clean.clone(),
                    post_clean: service.post_clean.clone(),
                })
            })
            .collect::<Vec<_>>();

        if services.is_empty() {
            return None;
        }

        Some(Self {
            env_vars: eph
                .env_vars
                .iter()
                .map(|variable| HookEnvVar {
                    name: variable.name.clone(),
                    value: variable.value.clone(),
                })
                .collect(),
            services,
        })
    }

    pub(crate) fn services_rev(&self) -> impl DoubleEndedIterator<Item = &TeardownHookService> {
        self.services.iter().rev()
    }

    pub(crate) fn environment(
        &self,
        workspace: HookWorkspace<'_>,
        running: &HashMap<String, RunningService>,
        service: &TeardownHookService,
    ) -> Result<Vec<(String, String)>, UnresolvedEnvironment> {
        hook_environment(
            workspace,
            self.env_vars
                .iter()
                .map(|variable| (variable.name.as_str(), variable.value.as_str())),
            running,
            service
                .env
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
    }
}

impl Service {
    fn has_teardown_hooks(&self) -> bool {
        !self.pre_stop.is_empty()
            || !self.post_stop.is_empty()
            || !self.pre_clean.is_empty()
            || !self.post_clean.is_empty()
    }
}

impl From<&ServiceSource> for CleanupKind {
    fn from(source: &ServiceSource) -> Self {
        match source {
            ServiceSource::Image(_) | ServiceSource::Dockerfile(_) => Self::DirectContainer,
            ServiceSource::Compose(_) => Self::Compose,
            ServiceSource::Command(_) => Self::Process,
        }
    }
}

/// Workspace identity injected into lifecycle hook environments.
#[derive(Clone, Copy)]
pub(crate) struct HookWorkspace<'a> {
    root: &'a Path,
    id: &'a str,
    short_id: &'a str,
}

impl<'a> HookWorkspace<'a> {
    pub(crate) fn new(root: &'a Path, id: &'a str, short_id: &'a str) -> Self {
        Self { root, id, short_id }
    }

    pub(crate) fn from_workspace(workspace: &'a Workspace) -> Self {
        Self::new(&workspace.path, &workspace.id, &workspace.short_id)
    }

    fn container_prefix(self) -> String {
        format!("eph-{}", self.short_id)
    }

    fn container_name(self, service: &str) -> String {
        format!("{}-{service}", self.container_prefix())
    }
}

/// A hook command that could not be spawned or exited unsuccessfully.
#[derive(Debug)]
pub(crate) struct HookFailure {
    pub(crate) command: String,
    kind: HookFailureKind,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Debug)]
enum HookFailureKind {
    Spawn(String),
    Exit(String),
}

impl HookFailure {
    /// Convert the structured failure back to the established lifecycle error text.
    pub(crate) fn into_lifecycle_error(self) -> anyhow::Error {
        match self.kind {
            HookFailureKind::Spawn(error) => {
                anyhow::anyhow!(error).context(format!("failed to execute hook: {}", self.command))
            }
            HookFailureKind::Exit(_) => {
                let mut detail = String::new();
                if !self.stdout.is_empty() {
                    detail.push_str("\nstdout:\n");
                    detail.push_str(&self.stdout);
                }
                if !self.stderr.is_empty() {
                    detail.push_str("\nstderr:\n");
                    detail.push_str(&self.stderr);
                }
                anyhow::anyhow!("hook failed: {}{}", self.command, detail)
            }
        }
    }
}

impl std::fmt::Display for HookFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            HookFailureKind::Spawn(error) => {
                write!(f, "{}: failed to execute hook: {error}", self.command)?;
            }
            HookFailureKind::Exit(status) => {
                write!(f, "{}: hook exited with {status}", self.command)?;
            }
        }
        if !self.stdout.is_empty() {
            write!(f, "\nstdout:\n{}", self.stdout)?;
        }
        if !self.stderr.is_empty() {
            write!(f, "\nstderr:\n{}", self.stderr)?;
        }
        Ok(())
    }
}

impl std::error::Error for HookFailure {}

/// Resolve the environment shared by ordinary and prune-driven hooks.
pub(crate) fn hook_environment<'a>(
    workspace: HookWorkspace<'_>,
    top_level: impl IntoIterator<Item = (&'a str, &'a str)>,
    running: &HashMap<String, RunningService>,
    service_env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<Vec<(String, String)>, UnresolvedEnvironment> {
    let mut env = resolve_env_pairs_strict(top_level, running)?;
    env.extend(metadata_environment(workspace, running));
    env.extend(resolve_env_pairs_strict(service_env, running)?);
    Ok(env)
}

fn metadata_environment(
    workspace: HookWorkspace<'_>,
    running: &HashMap<String, RunningService>,
) -> Vec<(String, String)> {
    let mut vars = vec![
        ("EPH_WORKSPACE_ID".to_string(), workspace.id.to_string()),
        (
            "EPH_WORKSPACE_ROOT".to_string(),
            workspace.root.display().to_string(),
        ),
        (
            "EPH_CONTAINER_PREFIX".to_string(),
            workspace.container_prefix(),
        ),
    ];

    for (name, service) in running {
        let key = name.to_uppercase().replace('-', "_");
        vars.push((format!("EPH_{key}_HOST"), service.host().to_string()));
        if let Some(port) = service.port() {
            vars.push((format!("EPH_{key}_PORT"), port.to_string()));
        }
        for (port_name, port) in &service.ports {
            if port_name != "default" {
                let port_key = port_name.to_uppercase().replace('-', "_");
                vars.push((format!("EPH_{key}_PORT_{port_key}"), port.to_string()));
            }
        }
        vars.push((
            format!("EPH_{key}_CONTAINER"),
            workspace.container_name(name),
        ));
    }

    vars
}

/// Run one hook through the platform shell without changing eph's inherited environment.
pub(crate) async fn run_hook(
    command: &str,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<(), HookFailure> {
    let output = proc::shell_command(command)
        .current_dir(cwd)
        .envs(
            env.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .output()
        .await
        .map_err(|error| HookFailure {
            command: command.to_string(),
            kind: HookFailureKind::Spawn(error.to_string()),
            stdout: String::new(),
            stderr: String::new(),
        })?;

    if output.status.success() {
        return Ok(());
    }

    Err(HookFailure {
        command: command.to_string(),
        kind: HookFailureKind::Exit(output.status.to_string()),
        stdout: String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
        stderr: String::from_utf8_lossy(&output.stderr)
            .trim_end()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[cfg(unix)]
    const FAILING_HOOK: &str = "printf 'out'; printf 'err' >&2; exit 7";
    #[cfg(windows)]
    const FAILING_HOOK: &str = "echo out& echo err 1>&2& exit /b 7";

    #[test]
    fn snapshot_keeps_only_teardown_hook_services_in_start_order() {
        let eph = parse(
            "[app]\nrun=app\npre-stop=stop-app\n\n[db]\nimage=postgres\n\n[cache]\nimage=redis\npost-clean=clean-cache\n",
        )
        .unwrap();

        let snapshot = TeardownHookSnapshot::capture(&eph).unwrap();
        let names = snapshot
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(names, ["cache", "app"]);
    }

    #[test]
    fn snapshot_is_absent_without_teardown_hooks() {
        let eph = parse("[db]\nimage=postgres\npost-start=seed\n").unwrap();

        assert!(TeardownHookSnapshot::capture(&eph).is_none());
    }

    #[test]
    fn snapshot_round_trips_without_startup_configuration() {
        let eph = parse(
            "DATABASE_URL=postgres://localhost:${db.port}/app\n\n[db]\nimage=postgres\nport=5432\nenv.POSTGRES_DB=app\npre-start=generate\npost-start=seed\npre-stop=backup\npost-clean=restore\n",
        )
        .unwrap();
        let snapshot = TeardownHookSnapshot::capture(&eph).unwrap();

        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: TeardownHookSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(restored, snapshot);
        assert!(!json.contains("pre_start"));
        assert!(!json.contains("post_start"));
        assert!(!json.contains("postgres:16"));
    }

    #[test]
    fn snapshot_deserialization_rejects_states_the_parser_cannot_create() {
        let valid = serde_json::json!({
            "env_vars": [{"name": "DATABASE_URL", "value": "postgres://localhost"}],
            "services": [{
                "name": "db",
                "cleanup_kind": "direct_container",
                "env": {"POSTGRES_DB": "app"},
                "pre_stop": ["backup"],
                "post_stop": [],
                "pre_clean": [],
                "post_clean": []
            }]
        });

        let mut cases = Vec::new();
        let mut no_services = valid.clone();
        no_services["services"] = serde_json::json!([]);
        cases.push(no_services);
        let mut invalid_service = valid.clone();
        invalid_service["services"][0]["name"] = serde_json::json!("DB");
        cases.push(invalid_service);
        let mut hookless = valid.clone();
        hookless["services"][0]["pre_stop"] = serde_json::json!([]);
        cases.push(hookless);
        let mut reserved_env = valid.clone();
        reserved_env["env_vars"][0]["name"] = serde_json::json!("EPH_WORKSPACE_ID");
        cases.push(reserved_env);

        for case in cases {
            assert!(serde_json::from_value::<TeardownHookSnapshot>(case).is_err());
        }

        let duplicate_service_env = r#"{
            "env_vars": [],
            "services": [{
                "name": "db",
                "cleanup_kind": "direct_container",
                "env": {"POSTGRES_DB": "app", "POSTGRES_DB": "other"},
                "pre_stop": ["backup"],
                "post_stop": [],
                "pre_clean": [],
                "post_clean": []
            }]
        }"#;
        assert!(serde_json::from_str::<TeardownHookSnapshot>(duplicate_service_env).is_err());
    }

    #[test]
    fn saved_snapshot_environment_resolves_ports_strictly() {
        let eph = parse(
            "DATABASE_URL=postgres://localhost:${db.port}/app\n\n[db]\nimage=postgres\nport=5432\nenv.PORT_COPY=${db.port}\npre-stop=backup\n",
        )
        .unwrap();
        let snapshot = TeardownHookSnapshot::capture(&eph).unwrap();
        let service = &snapshot.services[0];
        let running = HashMap::from([(
            "db".to_string(),
            RunningService {
                name: "db".to_string(),
                ports: HashMap::from([("default".to_string(), 15432)]),
            },
        )]);
        let root = Path::new("/recorded/workspace");
        let env = snapshot
            .environment(
                HookWorkspace::new(root, "full-id", "short-id"),
                &running,
                service,
            )
            .unwrap();

        assert!(env.contains(&(
            "DATABASE_URL".to_string(),
            "postgres://localhost:15432/app".to_string()
        )));
        assert!(env.contains(&("PORT_COPY".to_string(), "15432".to_string())));
        assert!(
            snapshot
                .environment(
                    HookWorkspace::new(root, "full-id", "short-id"),
                    &HashMap::new(),
                    service,
                )
                .is_err()
        );
    }

    #[test]
    fn hook_environment_reports_every_unresolved_variable() {
        let running = HashMap::new();
        let error = hook_environment(
            HookWorkspace::new(Path::new("/recorded/workspace"), "full-id", "short-id"),
            [
                ("DATABASE_URL", "postgres://localhost:${db.port}"),
                ("CACHE_URL", "redis://localhost:${cache.port}"),
            ],
            &running,
            std::iter::empty(),
        )
        .unwrap_err();

        assert_eq!(
            error
                .unresolved
                .iter()
                .map(|variable| variable.name.as_str())
                .collect::<Vec<_>>(),
            ["DATABASE_URL", "CACHE_URL"]
        );
    }

    #[tokio::test]
    async fn lifecycle_conversion_preserves_existing_hook_error_text() {
        let dir = tempfile::tempdir().unwrap();
        let failure = run_hook(FAILING_HOOK, dir.path(), &[]).await.unwrap_err();
        let error = failure.into_lifecycle_error().to_string();

        assert!(error.starts_with(&format!("hook failed: {FAILING_HOOK}")));
        assert!(error.contains("stdout:") && error.contains("stderr:"));
        assert!(!error.contains("hook exited with"));

        let missing = dir.path().join("missing");
        let failure = run_hook("ignored", &missing, &[]).await.unwrap_err();
        assert_eq!(
            failure.into_lifecycle_error().to_string(),
            "failed to execute hook: ignored"
        );
    }
}
