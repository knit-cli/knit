//! Validates a land plan against the resolved bundle and preflights its PRs
//! (open, ready, and checks not already failing) before any merge is attempted.

use super::{
    ensure_open_and_ready, ensure_provider, repo_context, required_repo_id, resolve_stored_path,
    resolve_subdir, run_step, state_is_merged, validate_deploy_checkout_update,
    validate_deployment_mode, validate_merge_method, LandPlan, LandRun, LandStep,
    DEPLOY_MODE_COMMAND, DEPLOY_MODE_PUSH, LAND_PLAN_KIND, STATUS_SUCCEEDED, STEP_DEPLOY,
    STEP_MERGE_PR, STEP_RUN, STEP_WAIT_CHECKS,
};
use crate::model::{DEFAULT_LANDING_MERGE_METHOD, SCHEMA_VERSION};
use crate::providers::{self, publication_for_repo, CheckRun, PrTarget};
use crate::store::ActiveBundle;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;

pub(super) fn validate_plan_for_bundle(active: &ActiveBundle, plan: &LandPlan) -> Result<()> {
    if plan.schema_version != SCHEMA_VERSION {
        bail!(
            "Land plan schemaVersion must be `{SCHEMA_VERSION}`, found `{}`.",
            plan.schema_version
        );
    }
    if plan.kind != LAND_PLAN_KIND {
        bail!(
            "Land plan kind must be `{LAND_PLAN_KIND}`, found `{}`.",
            plan.kind
        );
    }
    if plan.bundle_id != active.bundle.id {
        bail!(
            "Land plan belongs to bundle {}, but resolved bundle is {}.",
            plan.bundle_id,
            active.bundle.id
        );
    }
    ensure_provider(&plan.provider)?;
    ordered_step_ids(&plan.steps)?;

    for step in &plan.steps {
        match step.step_type.as_str() {
            STEP_MERGE_PR => {
                required_repo_id(step)?;
                validate_merge_method(
                    step.method
                        .as_deref()
                        .unwrap_or(DEFAULT_LANDING_MERGE_METHOD),
                )?;
            }
            STEP_WAIT_CHECKS => {
                required_repo_id(step)?;
            }
            STEP_RUN => {
                if step.command.is_empty() {
                    bail!("run step `{}` must provide command", step.id);
                }
                let cwd = step
                    .cwd
                    .as_deref()
                    .map(|cwd| resolve_stored_path(&active.root, cwd))
                    .unwrap_or_else(|| active.root.clone());
                if !cwd.exists() {
                    bail!(
                        "run step `{}` cwd does not exist: {}",
                        step.id,
                        cwd.display()
                    );
                }
            }
            STEP_DEPLOY => {
                required_repo_id(step)?;
                let mode = step
                    .deployment_mode
                    .as_deref()
                    .unwrap_or(if step.command.is_empty() {
                        DEPLOY_MODE_PUSH
                    } else {
                        DEPLOY_MODE_COMMAND
                    });
                validate_deployment_mode(mode)?;
                if mode == DEPLOY_MODE_COMMAND && step.command.is_empty() {
                    bail!("deploy step `{}` must provide command", step.id);
                }
                if mode == DEPLOY_MODE_PUSH && !step.command.is_empty() {
                    bail!(
                        "deploy step `{}` uses push mode and must not provide command",
                        step.id
                    );
                }
                if let Some(checkout) = &step.checkout {
                    if checkout.branch.trim().is_empty() {
                        bail!(
                            "deploy step `{}` checkout branch must not be empty",
                            step.id
                        );
                    }
                    validate_deploy_checkout_update(checkout.update.as_deref().unwrap_or("fetch"))?;
                } else if let Some(cwd) = &step.cwd {
                    let (_, _, checkout) = repo_context(active, required_repo_id(step)?)?;
                    let cwd = resolve_subdir(&checkout, cwd);
                    if !cwd.exists() {
                        bail!(
                            "deploy step `{}` cwd does not exist: {}",
                            step.id,
                            cwd.display()
                        );
                    }
                }
            }
            step_type => bail!("unknown land step type `{step_type}` in `{}`", step.id),
        }
    }
    Ok(())
}

pub(super) fn preflight_publications(
    active: &ActiveBundle,
    plan: &LandPlan,
    run: Option<&LandRun>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for step in &plan.steps {
        if !matches!(step.step_type.as_str(), STEP_MERGE_PR | STEP_WAIT_CHECKS) {
            continue;
        }
        if run
            .and_then(|run| run_step(run, &step.id))
            .is_some_and(|run_step| run_step.status == STATUS_SUCCEEDED)
        {
            continue;
        }
        let repo_id = required_repo_id(step)?;
        if !seen.insert(repo_id.to_string()) {
            continue;
        }
        let (_, repo, cwd) = repo_context(active, repo_id)?;
        let forge = providers::for_repo(&repo)?;
        let target = PrTarget::checkout(&cwd);
        let publication = publication_for_repo(&active.bundle, repo_id)
            .with_context(|| format!("{repo_id}: missing review publication"))?;
        let pr = forge.view(&target, &publication.url)?;
        // An already-merged PR is a satisfied step regardless of whether a prior
        // run exists; the executor and the from-artifact path both skip it.
        if state_is_merged(&pr) {
            continue;
        }
        ensure_open_and_ready(repo_id, &pr)?;
        if step.step_type == STEP_WAIT_CHECKS || step.wait_for_checks.unwrap_or(true) {
            let runs = forge.check_runs(
                &target,
                &publication.url,
                step.required_checks_only
                    .or(step.required_only)
                    .unwrap_or(true),
            )?;
            ensure_checks_ready(repo_id, &runs)?;
        }
    }
    Ok(())
}

fn ensure_checks_ready(repo_id: &str, runs: &[CheckRun]) -> Result<()> {
    let failed = runs
        .iter()
        .filter(|run| {
            matches!(run.bucket.as_deref(), Some("fail" | "cancel"))
                || matches!(run.state.as_deref(), Some("FAILURE" | "CANCELLED"))
        })
        .map(|run| run.name.as_str())
        .collect::<Vec<_>>();
    if !failed.is_empty() {
        bail!("{repo_id}: required checks failed: {}", failed.join(", "));
    }
    let pending = runs.iter().any(|run| {
        !matches!(run.bucket.as_deref(), Some("pass" | "skipping"))
            && !matches!(run.state.as_deref(), Some("SUCCESS" | "SKIPPED"))
    });
    if pending {
        bail!("{repo_id}: required checks are pending.");
    }
    Ok(())
}

pub(super) fn ordered_step_ids(steps: &[LandStep]) -> Result<Vec<String>> {
    if steps.is_empty() {
        bail!("land plan must contain at least one step");
    }
    let mut ids = BTreeSet::new();
    for step in steps {
        if step.id.trim().is_empty() {
            bail!("land step id must not be empty");
        }
        if !ids.insert(step.id.clone()) {
            bail!("duplicate land step id `{}`", step.id);
        }
    }

    let mut emitted = BTreeSet::new();
    let mut order = Vec::new();
    loop {
        let before = order.len();
        for step in steps {
            if emitted.contains(&step.id) {
                continue;
            }
            for need in &step.needs {
                if !ids.contains(need) {
                    bail!("step `{}` needs unknown step `{need}`", step.id);
                }
            }
            if step.needs.iter().all(|need| emitted.contains(need)) {
                emitted.insert(step.id.clone());
                order.push(step.id.clone());
            }
        }
        if order.len() == steps.len() {
            return Ok(order);
        }
        if order.len() == before {
            bail!("land plan contains a dependency cycle");
        }
    }
}
