//! The knit bundle runtime: "docker compose up in every bundle repo, with the
//! bundle's code". Lifts each compose-bearing repo into an isolated per-bundle
//! compose project (transform mode) or runs a `KNIT_*` contract compose file
//! as-is (contract mode), with cross-stack port rewiring and optional shared
//! dev database attachment.
//!
//! The crate is version-control agnostic on purpose: it consumes a
//! [`RuntimeContext`] — a workspace root, a bundle id, and repos with source
//! paths and checkout paths — plus the [`config::ProjectRuntime`] block. The
//! knit CLI adapts bundle state into that contract; anything else (an app
//! embedding the runtime, a future standalone binary) can do the same.

pub mod config;
mod eject;
mod envfile;
mod plan;
mod state;
mod support;
mod transform;
mod up;
mod yaml;

use anyhow::{bail, Result};
use std::path::PathBuf;

pub use plan::detect_stack_repo_ids;

/// Everything the runtime needs to know about a bundle, supplied by the
/// caller. Repo order is preserved (stacks start in this order).
pub struct RuntimeContext {
    /// Workspace root; run state lands under `.knit/runtime-runs/<bundle>/`.
    pub root: PathBuf,
    pub bundle_id: String,
    /// The bundle's repos: id, source checkout, and bundle checkout (worktree)
    /// when materialized.
    pub repos: Vec<RuntimeRepo>,
    /// Additional repo checkouts exposed through the `KNIT_CHECKOUT_*` env
    /// contract (project repos that are not in the bundle).
    pub extra_checkouts: Vec<(String, PathBuf)>,
}

#[derive(Clone)]
pub struct RuntimeRepo {
    pub id: String,
    pub source_path: PathBuf,
    pub checkout: Option<PathBuf>,
}

/// Build and start every requested stack. `stack_repo_ids` come from the
/// caller's resolution of `runtime.stacks`/`stackRepo`, or from
/// [`detect_stack_repo_ids`] for the zero-config path.
pub fn up(
    ctx: &RuntimeContext,
    runtime: &config::ProjectRuntime,
    stack_repo_ids: &[String],
) -> Result<()> {
    if runtime.kind != "docker-compose" {
        bail!("Unsupported runtime kind `{}`.", runtime.kind);
    }
    let plans = plan::build_stack_plans(ctx, runtime, stack_repo_ids)?;
    up::run_up_stacks(ctx, runtime, plans)
}

/// Materialize each transform-mode stack's lift as an editable
/// `docker-compose.knit.yml` in its repo checkout — the exit from transform
/// mode into contract mode, so a stack the automatic lift mishandles becomes
/// a committed file to fix instead of a knit change. Contract stacks are
/// skipped; existing files are only overwritten with `force`.
pub fn eject(
    ctx: &RuntimeContext,
    runtime: &config::ProjectRuntime,
    stack_repo_ids: &[String],
    force: bool,
) -> Result<()> {
    if runtime.kind != "docker-compose" {
        bail!("Unsupported runtime kind `{}`.", runtime.kind);
    }
    let plans = plan::build_stack_plans(ctx, runtime, stack_repo_ids)?;
    eject::run_eject(ctx, runtime, plans, force)
}

/// Stop and remove the bundle's stacks, resolved from recorded run state or
/// by derived compose project names when a failed `up` never recorded state.
pub fn down(ctx: &RuntimeContext) -> Result<()> {
    state::run_down(ctx)
}

/// Report live service states, ports, and URLs for the bundle's stacks.
pub fn status(ctx: &RuntimeContext) -> Result<()> {
    state::run_status(ctx)
}

/// Whether this bundle has recorded runtime run state.
pub fn has_state(ctx: &RuntimeContext) -> bool {
    state::has_state(ctx)
}
