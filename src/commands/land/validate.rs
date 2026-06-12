//! Validates a land plan against the resolved bundle and preflights its PRs
//! (open, ready, and checks not already failing) before any merge is attempted.

use super::{
    ensure_open_and_ready, ensure_provider, repo_context, required_repo_id, resolve_stored_path,
    resolve_subdir, run_step, state_is_merged, LandPlan, LandRun, LandStatus, LandStep,
    LandStepKind, LAND_PLAN_KIND,
};
use crate::model::{DeployMode, SCHEMA_VERSION};
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
        match step.step_type {
            LandStepKind::MergePr => {
                required_repo_id(step)?;
            }
            LandStepKind::WaitChecks => {
                required_repo_id(step)?;
            }
            LandStepKind::Run => {
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
            LandStepKind::Deploy => {
                required_repo_id(step)?;
                let mode = step.deployment_mode.unwrap_or(if step.command.is_empty() {
                    DeployMode::Push
                } else {
                    DeployMode::Command
                });
                if mode == DeployMode::Command && step.command.is_empty() {
                    bail!("deploy step `{}` must provide command", step.id);
                }
                if mode == DeployMode::Push && !step.command.is_empty() {
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
        if !matches!(
            step.step_type,
            LandStepKind::MergePr | LandStepKind::WaitChecks
        ) {
            continue;
        }
        if run
            .and_then(|run| run_step(run, &step.id))
            .is_some_and(|run_step| run_step.status == LandStatus::Succeeded)
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
        if step.step_type == LandStepKind::WaitChecks || step.wait_for_checks.unwrap_or(true) {
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
