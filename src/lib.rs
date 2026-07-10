//! `eph` - ephemeral services per workspace, like dotenv for services.
//!
//! This library holds the reusable logic behind the `eph` CLI: parsing `.eph`
//! files ([`parser`]), resolving a workspace from the filesystem
//! ([`workspace`]), managing Docker-backed services ([`service`]), pruning stale
//! cross-workspace resources ([`prune`]), rendering resolved environment
//! variables for shell `eval` ([`mod@env`]), installing bundled agent skills into
//! a consuming repo ([`skills`]), and replacing the running binary with the
//! latest GitHub release ([`update`]). The crate-internal `proc` module hides the
//! platform split for the shell and PID control (`sh -c`/`cmd /C`, native
//! liveness and teardown) so `run=` services and hooks work on Windows as well as
//! Unix. The binary in `main.rs` is a thin clap front end over these APIs.

#![warn(missing_docs)]
#![deny(clippy::correctness)]
#![warn(clippy::suspicious)]
#![warn(clippy::style)]
#![warn(clippy::complexity)]
#![warn(clippy::perf)]

pub mod env;
pub mod parser;
pub(crate) mod proc;
pub mod prune;
pub mod service;
pub mod skills;
pub mod update;
pub mod workspace;

pub use env::{
    escape_bash, escape_fish, escape_powershell, render, render_export, render_fish, render_json,
    render_powershell, render_with_unsets,
};
pub use parser::{EphFile, Service, ServiceSource, parse, resolve_interpolations};
pub use prune::{ConfirmationOutcome, PruneOptions, PruneReport, confirmation_outcome, prune};
pub use service::{
    Hooks, LogOptions, RunningService, ServiceManager, UnresolvedEnvVar, UnresolvedEnvironment,
    UnresolvedReference, resolve_against_strict, resolve_env_vars, resolve_env_vars_strict,
};
pub use workspace::Workspace;
