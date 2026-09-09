//! `knit run up|status|down|eject` — the adapter between knit bundle state and the
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
use knit_runtime::{EngineView, RuntimeContext, RuntimeRepo};
use std::path::PathBuf;

/// Where this process sees the engine's workspace volume, when
/// `KNIT_RUNTIME_ENGINE_VOLUME_MOUNT` does not say.
const DEFAULT_ENGINE_VOLUME_MOUNT: &str = "/var/lib/svartal";

pub fn try_handle(name: &str, force: bool, purge: bool, json: bool) -> Result<bool> {
    let active = load_active_bundle()?;
    let project = load_project_for_bundle(&active).ok();
    let runtime = project.as_ref().and_then(|p| p.runtime.clone());
    let ctx = runtime_context(&active, project.as_ref())?;

    match name {
        "up" => {
            let stack_repo_ids = resolve_stack_repo_ids(&ctx, runtime.as_ref())?;
            if stack_repo_ids.is_empty() {
                return Ok(false);
            }
            knit_runtime::up(&ctx, &runtime.unwrap_or_default(), &stack_repo_ids).map(|_| true)
        }
        "eject" => {
            let stack_repo_ids = resolve_stack_repo_ids(&ctx, runtime.as_ref())?;
            if stack_repo_ids.is_empty() {
                return Ok(false);
            }
            knit_runtime::eject(&ctx, &runtime.unwrap_or_default(), &stack_repo_ids, force)
                .map(|_| true)
        }
        "down" => {
            if !runtime_applies(&ctx, runtime.as_ref()) {
                return Ok(false);
            }
            if purge {
                knit_runtime::purge(&ctx).map(|_| true)
            } else {
                knit_runtime::down(&ctx).map(|_| true)
            }
        }
        "status" => {
            if !runtime_applies(&ctx, runtime.as_ref()) {
                return Ok(false);
            }
            if json {
                knit_runtime::status_json(&ctx).map(|_| true)
            } else {
                knit_runtime::status(&ctx).map(|_| true)
            }
        }
        _ => Ok(false),
    }
}

/// Remove all Docker resources owned by a bundle runtime before its generated
/// worktrees are discarded. Callers use this for terminal bundle lifecycle
/// transitions (archive/land/delete), where keeping restart data would only
/// leak project-scoped volumes and Compose build images.
pub(crate) fn purge_active_runtime(active: &ActiveBundle) -> Result<bool> {
    let project = load_project_for_bundle(active).ok();
    let runtime = project.as_ref().and_then(|project| project.runtime.clone());
    let ctx = runtime_context(active, project.as_ref())?;
    if !runtime_applies(&ctx, runtime.as_ref()) {
        return Ok(false);
    }
    knit_runtime::purge(&ctx)?;
    Ok(true)
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
fn runtime_context(active: &ActiveBundle, project: Option<&KnitProject>) -> Result<RuntimeContext> {
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
    Ok(RuntimeContext {
        root: active.root.clone(),
        bundle_id: active.bundle.id.clone(),
        repos,
        extra_checkouts,
        engine: engine_view()?,
    })
}

/// The engine view (docker outside of docker) from the environment a
/// supervisor starts knit with. `KNIT_RUNTIME_ENGINE_VOLUME` is the switch:
/// without it the runtime behaves exactly as it does on a laptop.
fn engine_view() -> Result<Option<EngineView>> {
    let Some(volume) = trimmed_env("KNIT_RUNTIME_ENGINE_VOLUME") else {
        return Ok(None);
    };
    if !is_volume_name(&volume) {
        bail!(
            "KNIT_RUNTIME_ENGINE_VOLUME `{volume}` is not a Docker volume name (expected `^[A-Za-z0-9][A-Za-z0-9_.-]{{0,127}}$`)."
        );
    }
    let mount = PathBuf::from(
        trimmed_env("KNIT_RUNTIME_ENGINE_VOLUME_MOUNT")
            .unwrap_or_else(|| DEFAULT_ENGINE_VOLUME_MOUNT.to_string()),
    );
    if !mount.is_absolute() {
        bail!(
            "KNIT_RUNTIME_ENGINE_VOLUME_MOUNT must be an absolute path, got `{}`.",
            mount.display()
        );
    }
    Ok(Some(EngineView {
        volume,
        mount,
        owner: trimmed_env("KNIT_RUNTIME_OWNER"),
    }))
}

fn trimmed_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$`, Docker's volume name grammar.
fn is_volume_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && value.len() <= 128
        && characters
            .all(|character| character.is_ascii_alphanumeric() || "_.-".contains(character))
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

#[cfg(test)]
mod tests {
    use super::is_volume_name;

    #[test]
    fn volume_names_follow_dockers_grammar() {
        assert!(is_volume_name("svartal-ws-1"));
        assert!(is_volume_name("a"));
        assert!(is_volume_name("A_b.c-d0"));
        assert!(!is_volume_name(""));
        assert!(!is_volume_name("-leading-dash"));
        assert!(!is_volume_name("has space"));
        assert!(!is_volume_name("has/slash"));
        assert!(!is_volume_name(&"a".repeat(129)));
        assert!(is_volume_name(&"a".repeat(128)));
    }
}
