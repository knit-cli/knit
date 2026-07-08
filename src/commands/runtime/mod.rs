//! `knit run up|status|down` — the adapter between knit bundle state and the
//! `knit-runtime` crate, which owns the actual per-bundle docker-compose
//! runtime (see that crate for semantics). This module resolves the active
//! bundle and project, translates them into the crate's [`RuntimeContext`]
//! contract, and applies knit-side config semantics (`runtime.stacks`
//! narrowing, legacy `stackRepo`, repo-id slugging). Keeping the runtime
//! behind that contract keeps knit's core version-control shaped.

use crate::checkout::checkout_dir;
use crate::model::{KnitProject, ProjectRuntime};
use crate::store::{load_active_bundle, project_path, read_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use knit_runtime::{RuntimeContext, RuntimeRepo};
use std::path::PathBuf;

pub fn try_handle(name: &str) -> Result<bool> {
    let active = load_active_bundle()?;
    let project = load_project_for_bundle(&active).ok();
    let runtime = project.as_ref().and_then(|p| p.runtime.clone());
    let ctx = runtime_context(&active, project.as_ref());

    match name {
        "up" => {
            let stack_repo_ids = resolve_stack_repo_ids(&ctx, runtime.as_ref())?;
            if stack_repo_ids.is_empty() {
                return Ok(false);
            }
            knit_runtime::up(&ctx, &runtime.unwrap_or_default(), &stack_repo_ids).map(|_| true)
        }
        "down" => {
            if !runtime_applies(&ctx, runtime.as_ref()) {
                return Ok(false);
            }
            knit_runtime::down(&ctx).map(|_| true)
        }
        "status" => {
            if !runtime_applies(&ctx, runtime.as_ref()) {
                return Ok(false);
            }
            knit_runtime::status(&ctx).map(|_| true)
        }
        _ => Ok(false),
    }
}

/// Whether `down`/`status` should handle this bundle: a configured runtime,
/// recorded run state, or a detectable stack repo (so cleanup works even when
/// a failed `up` never recorded state).
fn runtime_applies(ctx: &RuntimeContext, runtime: Option<&ProjectRuntime>) -> bool {
    runtime.is_some()
        || knit_runtime::has_state(ctx)
        || !knit_runtime::detect_stack_repo_ids(ctx).is_empty()
}

/// The bundle repos whose stacks `up` lifts. `runtime.stacks` narrows to an
/// explicit set (repos absent from this bundle are skipped, so narrowed
/// bundles run what they contain); the legacy `stackRepo` forces one stack;
/// otherwise every bundle repo with a compose file is a stack.
fn resolve_stack_repo_ids(
    ctx: &RuntimeContext,
    runtime: Option<&ProjectRuntime>,
) -> Result<Vec<String>> {
    if let Some(runtime) = runtime {
        if !runtime.stacks.is_empty() {
            return Ok(runtime
                .stacks
                .iter()
                .map(|id| crate::ids::slugify(id))
                .filter(|slug| ctx.repos.iter().any(|repo| repo.id == *slug))
                .collect());
        }
        if let Some(stack_repo_id) = &runtime.stack_repo {
            if !ctx.repos.iter().any(|repo| repo.id == *stack_repo_id) {
                bail!("stack repo `{stack_repo_id}` is not tracked in this bundle");
            }
            return Ok(vec![stack_repo_id.clone()]);
        }
    }
    Ok(knit_runtime::detect_stack_repo_ids(ctx))
}

/// Translate the active bundle (plus project repos, for the `KNIT_*` env
/// contract) into the runtime crate's context.
fn runtime_context(active: &ActiveBundle, project: Option<&KnitProject>) -> RuntimeContext {
    let repos = active
        .bundle
        .repos
        .iter()
        .map(|repo| RuntimeRepo {
            id: repo.id.clone(),
            source_path: PathBuf::from(&repo.path),
            checkout: checkout_dir(active, repo),
        })
        .collect();
    let extra_checkouts = project
        .map(|project| {
            project
                .repos
                .iter()
                .map(|repo| (repo.id.clone(), PathBuf::from(&repo.path)))
                .collect()
        })
        .unwrap_or_default();
    RuntimeContext {
        root: active.root.clone(),
        bundle_id: active.bundle.id.clone(),
        repos,
        extra_checkouts,
    }
}

fn load_project_for_bundle(active: &ActiveBundle) -> Result<KnitProject> {
    let config = crate::store::load_config(&active.root)?;
    let project_id = active
        .bundle
        .project_id
        .as_deref()
        .or(config.active_project.as_deref())
        .context("The resolved bundle is not associated with a Knit project.")?;
    read_json(&project_path(&active.root, project_id))
}
