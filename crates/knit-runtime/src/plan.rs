//! Stack planning: which compose file each stack repo runs, in which mode,
//! under which compose project name.

use crate::config::{ProjectRuntime, RuntimeMode};
use crate::state::compose_project_name;
use crate::{RuntimeContext, RuntimeRepo};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const CONTRACT_COMPOSE_CANDIDATES: [&str; 1] = ["docker-compose.knit.yml"];
pub(crate) const PLAIN_COMPOSE_CANDIDATES: [&str; 4] = [
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yaml",
    "compose.yml",
];

/// One stack `up` will start: a bundle repo, its checkout, the compose file
/// to lift, the detected mode, and the compose project name the containers
/// run under.
pub(crate) struct StackPlan {
    pub(crate) repo: RuntimeRepo,
    pub(crate) checkout: PathBuf,
    pub(crate) compose: PathBuf,
    pub(crate) mode: RuntimeMode,
    pub(crate) project_name: String,
}

/// Bundle repos whose checkout contains a compose file — the auto-detected
/// stack set.
pub fn detect_stack_repo_ids(ctx: &RuntimeContext) -> Vec<String> {
    ctx.repos
        .iter()
        .filter(|repo| {
            repo.checkout.as_deref().is_some_and(|checkout| {
                CONTRACT_COMPOSE_CANDIDATES
                    .iter()
                    .chain(PLAIN_COMPOSE_CANDIDATES.iter())
                    .any(|candidate| checkout.join(candidate).exists())
            })
        })
        .map(|repo| repo.id.clone())
        .collect()
}

pub(crate) fn build_stack_plans(
    ctx: &RuntimeContext,
    runtime: &ProjectRuntime,
    stack_repo_ids: &[String],
) -> Result<Vec<StackPlan>> {
    let multi = stack_repo_ids.len() > 1;
    stack_repo_ids
        .iter()
        .map(|repo_id| {
            let repo = ctx
                .repos
                .iter()
                .find(|repo| repo.id == *repo_id)
                .with_context(|| format!("repo `{repo_id}` is not tracked in this bundle"))?;
            let checkout = repo.checkout.clone().with_context(|| {
                format!(
                    "{} has no checkout. Run `knit bundle worktree` first.",
                    repo.id
                )
            })?;
            // A configured `composeFile` names a file inside the configured
            // stack repo; every other stack uses its own default compose file.
            let compose = if runtime.compose_file.is_some()
                && (!multi || runtime.stack_repo.as_deref() == Some(repo.id.as_str()))
            {
                find_compose_file(&checkout, runtime)?
            } else {
                find_default_compose_file(&checkout)?
            };
            let mode = detect_mode(runtime, &compose)?;
            let project_name = if multi {
                format!("{}--{}", compose_project_name(&ctx.bundle_id), repo.id)
            } else {
                compose_project_name(&ctx.bundle_id)
            };
            Ok(StackPlan {
                repo: repo.clone(),
                checkout,
                compose,
                mode,
                project_name,
            })
        })
        .collect()
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
    find_default_compose_file(stack_checkout)
}

fn find_default_compose_file(stack_checkout: &Path) -> Result<PathBuf> {
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
