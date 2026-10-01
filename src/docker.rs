//! Connecting to the Docker daemon the `docker` CLI would use.
//!
//! bollard's local defaults read only a `unix://` `DOCKER_HOST` and otherwise
//! use the platform's default socket (`/var/run/docker.sock`, or the
//! `docker_engine` named pipe on Windows). Colima, OrbStack, Rancher Desktop, and rootless Docker select their
//! daemon through a docker CLI *context* instead and leave that socket absent,
//! so `docker ps` works while eph cannot connect. This module resolves the
//! endpoint the way the CLI does:
//!
//! 1. `DOCKER_HOST`, when set and non-empty, turns off context lookup and
//!    leaves the choice to bollard's defaults, as before contexts were read.
//! 2. Otherwise the context named by `DOCKER_CONTEXT`, else `currentContext` in
//!    `$DOCKER_CONFIG/config.json` (default `~/.docker/config.json`).
//! 3. The `default` context, or no context at all, means bollard's defaults.
//! 4. Any other context's endpoint is `Endpoints.docker.Host` in
//!    `$DOCKER_CONFIG/contexts/meta/<sha256(name)>/meta.json`.
//!
//! eph talks to a context only over a local socket. A context that points at a
//! remote (`ssh://`, `tcp://`) daemon, or that is selected but has no metadata,
//! is an error naming the context rather than a silent fallback to a different
//! daemon than the one `docker` uses.

use anyhow::{Context, Result, bail};
use bollard::Docker;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Connect to the daemon the `docker` CLI would use and confirm it answers.
pub(crate) async fn connect() -> Result<Docker> {
    let endpoint = resolve(&Selection::from_env())?;
    let (client, via) = match &endpoint {
        Endpoint::Defaults => (
            Docker::connect_with_local_defaults().map_err(anyhow::Error::from),
            String::new(),
        ),
        Endpoint::Context { name, socket } => {
            debug!("Using docker context `{name}` at {}", socket.host());
            (
                socket.connect(),
                format!(" through docker context `{name}` at {}", socket.host()),
            )
        }
    };
    let client =
        client.with_context(|| format!("failed to connect to docker (is docker running?){via}"))?;
    client
        .ping()
        .await
        .with_context(|| format!("failed to ping docker daemon{via}"))?;
    Ok(client)
}

/// The inputs the docker CLI consults to choose a daemon, read once from the
/// process environment so resolution itself is a pure function of them.
#[derive(Debug, Default)]
struct Selection {
    /// `DOCKER_HOST`, if set and non-empty.
    docker_host: Option<String>,
    /// `DOCKER_CONTEXT`, if set and non-empty.
    docker_context: Option<String>,
    /// `$DOCKER_CONFIG`, else `~/.docker`; `None` when neither is known.
    config_dir: Option<PathBuf>,
}

impl Selection {
    fn from_env() -> Self {
        Self {
            docker_host: env_nonempty("DOCKER_HOST"),
            docker_context: env_nonempty("DOCKER_CONTEXT"),
            config_dir: env_nonempty("DOCKER_CONFIG")
                .map(PathBuf::from)
                .or_else(|| dirs::home_dir().map(|home| home.join(".docker"))),
        }
    }
}

/// Read an environment variable, treating unset or empty as absent, as the
/// docker CLI does.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The daemon eph will connect to.
#[derive(Debug, PartialEq, Eq)]
enum Endpoint {
    /// `DOCKER_HOST`, or no context selected: bollard's defaults decide.
    Defaults,
    /// A named docker CLI context with a local socket endpoint.
    Context { name: String, socket: Socket },
}

/// A context's local endpoint. Both kinds parse on every platform so a
/// context's metadata means the same thing everywhere; only connecting is
/// platform-specific.
#[derive(Debug, PartialEq, Eq)]
enum Socket {
    /// `unix://<path>`.
    Unix(String),
    /// `npipe://<path>`.
    NamedPipe(String),
}

impl Socket {
    /// Parse a context's `Endpoints.docker.Host`, refusing endpoints eph
    /// cannot reach.
    fn parse(context: &str, host: String) -> Result<Self> {
        if host.starts_with("unix://") {
            Ok(Socket::Unix(host))
        } else if host.starts_with("npipe://") {
            Ok(Socket::NamedPipe(host))
        } else {
            bail!(
                "docker context `{context}` points at {host}, which eph cannot reach (eph \
                 connects only to a local unix socket or named pipe); set DOCKER_HOST to a \
                 local socket or switch contexts with `docker context use`"
            )
        }
    }

    fn host(&self) -> &str {
        match self {
            Socket::Unix(host) | Socket::NamedPipe(host) => host,
        }
    }

    fn connect(&self) -> Result<Docker> {
        let timeout = DEFAULT_TIMEOUT_SECS;
        let version = bollard::API_DEFAULT_VERSION;
        match self {
            #[cfg(unix)]
            Socket::Unix(host) => Ok(Docker::connect_with_unix(host, timeout, version)?),
            #[cfg(windows)]
            Socket::NamedPipe(host) => Ok(Docker::connect_with_named_pipe(host, timeout, version)?),
            #[cfg(unix)]
            Socket::NamedPipe(host) => bail!("{host} is a Windows named pipe"),
            #[cfg(windows)]
            Socket::Unix(host) => {
                bail!("{host} is a unix socket, which eph does not support on Windows")
            }
        }
    }
}

/// bollard's default request timeout, which it does not export.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// The part of the docker CLI's `config.json` that selects a context.
#[derive(Deserialize)]
struct CliConfig {
    #[serde(rename = "currentContext", default)]
    current_context: Option<String>,
}

/// The part of a context's `meta.json` that names its daemon.
#[derive(Deserialize)]
struct ContextMeta {
    #[serde(rename = "Endpoints")]
    endpoints: ContextEndpoints,
}

#[derive(Deserialize)]
struct ContextEndpoints {
    docker: DockerEndpoint,
}

#[derive(Deserialize)]
struct DockerEndpoint {
    #[serde(rename = "Host")]
    host: String,
}

/// Decide which daemon to connect to, in the docker CLI's order.
fn resolve(selection: &Selection) -> Result<Endpoint> {
    if selection.docker_host.is_some() {
        return Ok(Endpoint::Defaults);
    }
    let Some(config_dir) = &selection.config_dir else {
        return Ok(Endpoint::Defaults);
    };
    let name = match &selection.docker_context {
        Some(name) => name.clone(),
        None => match current_context(config_dir) {
            Some(name) => name,
            None => return Ok(Endpoint::Defaults),
        },
    };
    if name == "default" {
        return Ok(Endpoint::Defaults);
    }
    let socket = Socket::parse(&name, context_host(config_dir, &name)?)?;
    Ok(Endpoint::Context { name, socket })
}

/// The `currentContext` recorded in `config.json`, if any.
///
/// Like the docker CLI, an unreadable or malformed `config.json` is a warning
/// rather than an error: a root-owned file left by `sudo docker login` must not
/// stop eph from reaching the default daemon.
fn current_context(config_dir: &Path) -> Option<String> {
    let path = config_dir.join("config.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!(
                "ignoring unreadable docker config {}: {err}",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_str::<CliConfig>(&text) {
        Ok(config) => config.current_context.filter(|name| !name.is_empty()),
        Err(err) => {
            warn!("ignoring malformed docker config {}: {err}", path.display());
            None
        }
    }
}

/// The `Endpoints.docker.Host` of the named context.
fn context_host(config_dir: &Path, name: &str) -> Result<String> {
    let path = config_dir
        .join("contexts")
        .join("meta")
        .join(hex::encode(Sha256::digest(name.as_bytes())))
        .join("meta.json");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "docker context `{name}` is selected but its metadata ({}) is unreadable; check \
             `docker context ls`, switch contexts with `docker context use`, or set DOCKER_HOST",
            path.display()
        )
    })?;
    let meta: ContextMeta = serde_json::from_str(&text).with_context(|| {
        format!(
            "failed to parse docker context `{name}` metadata: {}",
            path.display()
        )
    })?;
    Ok(meta.endpoints.docker.host)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLIMA: &str = "unix:///home/me/.colima/default/docker.sock";

    /// A selection with no environment overrides, reading `config_dir`.
    fn selection(config_dir: &Path) -> Selection {
        Selection {
            config_dir: Some(config_dir.to_path_buf()),
            ..Selection::default()
        }
    }

    fn write_config(config_dir: &Path, json: &str) {
        std::fs::create_dir_all(config_dir).unwrap();
        std::fs::write(config_dir.join("config.json"), json).unwrap();
    }

    /// Write a context's metadata the way `docker context create` lays it out.
    fn write_context(config_dir: &Path, name: &str, host: &str) {
        let dir = config_dir
            .join("contexts")
            .join("meta")
            .join(hex::encode(Sha256::digest(name.as_bytes())));
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "Name": name,
            "Metadata": { "Description": name },
            "Endpoints": { "docker": { "Host": host, "SkipTLSVerify": false } },
        });
        std::fs::write(dir.join("meta.json"), meta.to_string()).unwrap();
    }

    fn colima() -> Endpoint {
        Endpoint::Context {
            name: "colima".into(),
            socket: Socket::Unix(COLIMA.into()),
        }
    }

    #[test]
    fn no_config_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve(&selection(dir.path())).unwrap(), Endpoint::Defaults);
        assert_eq!(resolve(&Selection::default()).unwrap(), Endpoint::Defaults);
    }

    #[test]
    fn context_metadata_is_found_by_the_docker_cli_digest() {
        // The CLI stores a context under the hex SHA-256 of its name. The
        // helpers above share the hashing with `context_host`, so pin the
        // directory a real `colima` context is stored in.
        assert_eq!(
            hex::encode(Sha256::digest(b"colima")),
            "f24fd3749c1368328e2b149bec149cb6795619f244c5b584e844961215dadd16"
        );
    }

    #[test]
    fn current_context_resolves_to_its_socket() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), r#"{"auths":{},"currentContext":"colima"}"#);
        write_context(dir.path(), "colima", COLIMA);
        assert_eq!(resolve(&selection(dir.path())).unwrap(), colima());
    }

    #[test]
    fn docker_host_overrides_the_context() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), r#"{"currentContext":"colima"}"#);
        write_context(dir.path(), "colima", COLIMA);
        let selection = Selection {
            docker_host: Some("unix:///tmp/other.sock".into()),
            ..selection(dir.path())
        };
        assert_eq!(resolve(&selection).unwrap(), Endpoint::Defaults);
    }

    #[test]
    fn docker_context_env_overrides_config() {
        let dir = tempfile::tempdir().unwrap();
        let orbstack = "unix:///home/me/.orbstack/run/docker.sock";
        write_config(dir.path(), r#"{"currentContext":"colima"}"#);
        write_context(dir.path(), "orbstack", orbstack);
        let selection = Selection {
            docker_context: Some("orbstack".into()),
            ..selection(dir.path())
        };
        assert_eq!(
            resolve(&selection).unwrap(),
            Endpoint::Context {
                name: "orbstack".into(),
                socket: Socket::Unix(orbstack.into()),
            }
        );
    }

    #[test]
    fn default_context_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), r#"{"currentContext":"default"}"#);
        assert_eq!(resolve(&selection(dir.path())).unwrap(), Endpoint::Defaults);
        let selection = Selection {
            docker_context: Some("default".into()),
            ..selection(dir.path())
        };
        assert_eq!(resolve(&selection).unwrap(), Endpoint::Defaults);
    }

    #[test]
    fn named_pipe_context_resolves_on_every_platform() {
        let dir = tempfile::tempdir().unwrap();
        let pipe = "npipe:////./pipe/dockerDesktopLinuxEngine";
        write_config(dir.path(), r#"{"currentContext":"desktop-linux"}"#);
        write_context(dir.path(), "desktop-linux", pipe);
        assert_eq!(
            resolve(&selection(dir.path())).unwrap(),
            Endpoint::Context {
                name: "desktop-linux".into(),
                socket: Socket::NamedPipe(pipe.into()),
            }
        );
    }

    #[test]
    fn malformed_config_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "{not json");
        assert_eq!(resolve(&selection(dir.path())).unwrap(), Endpoint::Defaults);
    }

    #[test]
    fn a_selected_but_missing_context_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), r#"{"currentContext":"gone"}"#);
        let err = resolve(&selection(dir.path())).unwrap_err();
        assert!(
            format!("{err:#}").contains("docker context `gone` is selected"),
            "{err:#}"
        );
    }

    #[test]
    fn a_remote_context_is_refused_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        write_context(dir.path(), "remote", "ssh://me@box");
        let selection = Selection {
            docker_context: Some("remote".into()),
            ..selection(dir.path())
        };
        let err = format!("{:#}", resolve(&selection).unwrap_err());
        assert!(err.contains("docker context `remote`"), "{err}");
        assert!(err.contains("set DOCKER_HOST"), "{err}");
    }
}
