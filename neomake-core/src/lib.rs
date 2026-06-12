//! Core library for `neomake` — a TOML/TOMLX-driven parallel build
//! orchestrator.
//!
//! `neomake-core` provides the pieces that do not care whether a config
//! was written in plain TOML or in the TOMLX superset: the task model,
//! the dependency-graph builder, the content-addressable cache, and the
//! tokio-based parallel executor. The binary crate and the TOMLX crate
//! are thin layers that feed this core.
//!
//! # Crates in the workspace
//!
//! - [`neomake-core`](crate) — this crate.
//! - `neomake-tomlx` — lexer/parser/evaluator for the TOMLX syntax.
//! - `neomake` — the `neomake` CLI binary.
//!
//! # Quick tour
//!
//! 1. Parse a config file (via `neomake-tomlx` or [`task::TaskSet::from_toml_file`]).
//! 2. Build a DAG with [`dag::Dag::build`].
//! 3. Open a [`cache::Cache`] rooted at the project directory.
//! 4. Call [`executor::run`] with selected tasks, the cache, and [`executor::RunOptions`].

#![warn(missing_docs)]
#![deny(clippy::all)]

pub mod cache;
pub mod dag;
pub mod error;
pub mod executor;
pub mod shell;
pub mod task;
pub mod asset;

pub use cache::{Cache, CacheEntry, CacheStatus, OutputRecord};
pub use dag::Dag;
pub use error::{CacheError, ConfigError, CoreError, DagError, ExecError};
pub use executor::{
    run, select_tasks, ExecutionEvent, Reporter, RunOptions, RunReport, TaskOutcome, TaskStatus,
};
pub use shell::{resolve as resolve_shell, ShellInvocation};
pub use task::{project_root_for, Task, TaskSet};
