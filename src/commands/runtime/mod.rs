//! `knit run up|status|down` — disposable per-bundle runtime instances.
//!
//! The goal: take the docker shape the repos already run on their main
//! branches and lift a parallel instance of that same shape — different
//! ports, isolated compose project, and the bundle's code substituted in.
//!
//! Two modes, picked per compose file:
//!
//! - **Transform** (default): the stack repo's own compose file is resolved
//!   with `docker compose config` and rewritten — paths into tracked repos
//!   remapped to bundle worktrees, published host ports reallocated, port
//!   references inside env values rewritten. No authoring required; see
//!   [`transform`].
//! - **Contract**: a `KNIT_*`-aware compose file is run as-is with the
//!   contract injected: `KNIT_ROOT`/`KNIT_BUNDLE`,
//!   `KNIT_CHECKOUT_<REPO>`/`KNIT_SRC_<REPO>`/`KNIT_REV_<REPO>` per repo,
//!   `KNIT_PORT_<SERVICE>` per configured port pool (backend/frontend by
//!   default), and `KNIT_DB_*`. Repo and service ids are uppercased with
//!   non-alphanumerics mapped to `_`. In `bundle` database mode the
//!   `bundle-db` compose profile is activated.
//!
//! Contract mode is chosen by the `runtime.mode` project setting, the
//! `docker-compose.knit.yml` filename, or `${KNIT_*}` references in the
//! compose file; everything else is lifted by transformation.
//!
//! Either way the stack runs as compose project `knit-run-<bundle>`, so
//! networks and named volumes are isolated per bundle, and `down`/`status`
//! resolve containers by label and survive worktree deletion.

mod state;
mod transform;
mod up;

use crate::checkout::checkout_dir;
use crate::model::{KnitProject, ProjectRuntime, RepoEntry, RuntimeMode};
use crate::store::{load_active_bundle, project_path, read_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use state::{has_state, run_down, run_status};
use std::fs;
use std::path::{Path, PathBuf};
use up::{run_up_contract, run_up_transform};

const RUNTIME_KIND_DOCKER_COMPOSE: &str = "docker-compose";
const CONTRACT_COMPOSE_CANDIDATES: [&str; 1] = ["docker-compose.knit.yml"];
const PLAIN_COMPOSE_CANDIDATES: [&str; 4] = [
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yaml",
    "compose.yml",
];

pub fn try_handle(name: &str) -> Result<bool> {
    let active = load_active_bundle()?;
    let project = load_project_for_bundle(&active).ok();
    let runtime = project.as_ref().and_then(|p| p.runtime.clone());

    match name {
        "up" => {
            let Some((runtime, stack_repo)) = resolve_runtime(&active, runtime)? else {
                return Ok(false);
            };
            run_up(&active, project.as_ref(), &runtime, stack_repo).map(|_| true)
        }
        "down" => {
            if !runtime_applies(&active, runtime)? {
                return Ok(false);
            }
            run_down(&active).map(|_| true)
        }
        "status" => {
            if !runtime_applies(&active, runtime)? {
                return Ok(false);
            }
            run_status(&active).map(|_| true)
        }
        _ => Ok(false),
    }
}

/// Whether `down`/`status` should handle this bundle: a configured runtime,
/// recorded run state, or a detectable stack repo (so cleanup works even when
/// a failed `up` never recorded state).
fn runtime_applies(active: &ActiveBundle, runtime: Option<ProjectRuntime>) -> Result<bool> {
    if runtime.is_some() || has_state(active) {
        return Ok(true);
    }
    Ok(resolve_runtime(active, None).unwrap_or(None).is_some())
}

/// Resolve the runtime config and stack repo for `up`. With no `runtime`
/// block in the project, fall back to auto-detection: a single bundle repo
/// with a compose file makes the runtime work with zero configuration.
fn resolve_runtime(
    active: &ActiveBundle,
    runtime: Option<ProjectRuntime>,
) -> Result<Option<(ProjectRuntime, &RepoEntry)>> {
    if let Some(stack_repo_id) = runtime.as_ref().and_then(|r| r.stack_repo.clone()) {
        let repo = active
            .bundle
            .repos
            .iter()
            .find(|repo| repo.id == stack_repo_id)
            .with_context(|| {
                format!("stack repo `{stack_repo_id}` is not tracked in this bundle")
            })?;
        return Ok(Some((runtime.unwrap(), repo)));
    }

    let mut detected = Vec::new();
    for repo in &active.bundle.repos {
        let Some(checkout) = checkout_dir(active, repo) else {
            continue;
        };
        let has_compose = CONTRACT_COMPOSE_CANDIDATES
            .iter()
            .chain(PLAIN_COMPOSE_CANDIDATES.iter())
            .any(|candidate| checkout.join(candidate).exists());
        if has_compose {
            detected.push(repo);
        }
    }

    match detected.len() {
        0 => Ok(None),
        1 => Ok(Some((runtime.unwrap_or_default(), detected[0]))),
        _ => bail!(
            "Multiple bundle repos have compose files ({}). Set `runtime.stackRepo` in the project to pick one.",
            detected
                .iter()
                .map(|repo| repo.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn run_up(
    active: &ActiveBundle,
    project: Option<&KnitProject>,
    runtime: &ProjectRuntime,
    stack_repo: &RepoEntry,
) -> Result<()> {
    if runtime.kind != RUNTIME_KIND_DOCKER_COMPOSE {
        bail!("Unsupported runtime kind `{}`.", runtime.kind);
    }

    let stack_checkout = checkout_dir(active, stack_repo).with_context(|| {
        format!(
            "{} has no checkout. Run `knit bundle worktree` first.",
            stack_repo.id
        )
    })?;

    let compose_path = find_compose_file(&stack_checkout, runtime)?;
    let mode = detect_mode(runtime, &compose_path)?;

    match mode {
        RuntimeMode::Contract => run_up_contract(
            active,
            project,
            runtime,
            stack_repo,
            &stack_checkout,
            &compose_path,
        ),
        RuntimeMode::Transform => {
            run_up_transform(active, runtime, stack_repo, &stack_checkout, &compose_path)
        }
    }
}

/// Pick the compose file: explicit `composeFile` config wins, then the
/// contract file, then the repo's own compose file.
fn find_compose_file(stack_checkout: &Path, runtime: &ProjectRuntime) -> Result<PathBuf> {
    if let Some(configured) = &runtime.compose_file {
        let path = stack_checkout.join(configured);
        if !path.exists() {
            bail!(
                "Runtime compose file not found: {}. Configured as `composeFile` in the project runtime.",
                path.display()
            );
        }
        return Ok(path);
    }
    for candidate in CONTRACT_COMPOSE_CANDIDATES
        .iter()
        .chain(PLAIN_COMPOSE_CANDIDATES.iter())
    {
        let path = stack_checkout.join(candidate);
        if path.exists() {
            return Ok(path);
        }
    }
    bail!(
        "No compose file found in {}. Add a docker-compose.yml to the stack repo (lifted automatically) or a docker-compose.knit.yml written against the KNIT_* contract.",
        stack_checkout.display()
    );
}

/// Explicit `runtime.mode` config wins; the contract filename or `${KNIT_*}`
/// variable references opt into the contract; anything else is lifted by
/// transformation.
fn detect_mode(runtime: &ProjectRuntime, compose_path: &Path) -> Result<RuntimeMode> {
    if let Some(mode) = runtime.mode {
        return Ok(mode);
    }
    if compose_path
        .file_name()
        .is_some_and(|name| CONTRACT_COMPOSE_CANDIDATES.iter().any(|c| name == *c))
    {
        return Ok(RuntimeMode::Contract);
    }
    let text = fs::read_to_string(compose_path)
        .with_context(|| format!("failed to read {}", compose_path.display()))?;
    if text.contains("${KNIT_") || text.contains("$KNIT_") {
        Ok(RuntimeMode::Contract)
    } else {
        Ok(RuntimeMode::Transform)
    }
}

/// Transform mode: resolve the repo's compose file in source-space, rewrite

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
    use super::*;

    #[test]
    fn detect_mode_sniffs_contract_variables() {
        let dir =
            std::env::temp_dir().join(format!("knit-runtime-mode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = ProjectRuntime::default();

        let contract = dir.join("contract.yml");
        std::fs::write(
            &contract,
            "services:\n  b:\n    ports: [\"${KNIT_PORT_BACKEND}:4000\"]\n",
        )
        .unwrap();
        let plain = dir.join("plain.yml");
        std::fs::write(&plain, "services:\n  b:\n    ports: [\"4000:4000\"]\n").unwrap();
        // A bare KNIT_ mention (e.g. in a comment) is not a contract opt-in.
        let comment = dir.join("comment.yml");
        std::fs::write(
            &comment,
            "# managed via KNIT_ tooling\nservices:\n  b:\n    ports: [\"4000:4000\"]\n",
        )
        .unwrap();
        let named = dir.join(CONTRACT_COMPOSE_CANDIDATES[0]);
        std::fs::write(&named, "services:\n  b:\n    ports: [\"4000:4000\"]\n").unwrap();

        assert_eq!(
            detect_mode(&runtime, &contract).unwrap(),
            RuntimeMode::Contract
        );
        assert_eq!(
            detect_mode(&runtime, &plain).unwrap(),
            RuntimeMode::Transform
        );
        assert_eq!(
            detect_mode(&runtime, &comment).unwrap(),
            RuntimeMode::Transform
        );
        assert_eq!(
            detect_mode(&runtime, &named).unwrap(),
            RuntimeMode::Contract
        );

        // Explicit config wins over detection.
        let forced = ProjectRuntime {
            mode: Some(RuntimeMode::Contract),
            ..Default::default()
        };
        assert_eq!(detect_mode(&forced, &plain).unwrap(), RuntimeMode::Contract);

        std::fs::remove_dir_all(dir).unwrap();
    }
}
