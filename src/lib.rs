//! `eph` - ephemeral services per workspace, like dotenv for services.
//!
//! This library holds the reusable logic behind the `eph` CLI: parsing `.eph`
//! files ([`parser`]), resolving a workspace from the filesystem
//! ([`workspace`]), managing Docker-backed services ([`service`]), rendering
//! resolved environment variables for shell `eval` ([`mod@env`]), and the agent
//! skills bundled into the binary and installed into a consuming repo
//! ([`skills`]), and replacing the running binary with the latest GitHub release
//! ([`update`]). The crate-internal `proc` module hides the platform split for
//! the shell and PID control (`sh -c`/`cmd /C`, native liveness and teardown) so
//! `run=` services and hooks work on Windows as well as Unix. The binary in
//! `main.rs` is a thin clap front end over these APIs.

#![warn(missing_docs)]
#![deny(clippy::correctness)]
#![warn(clippy::suspicious)]
#![warn(clippy::style)]
#![warn(clippy::complexity)]
#![warn(clippy::perf)]

pub mod env;
pub mod parser;
pub(crate) mod proc;
pub mod service;
pub mod skills;
pub mod update;
pub mod workspace;

pub use env::{escape_bash, escape_fish, render, render_export, render_fish, render_json};
pub use parser::{EphFile, Service, ServiceSource, parse, resolve_interpolations};
pub use service::{LogOptions, RunningService, ServiceManager, resolve_env_vars};
pub use workspace::Workspace;
