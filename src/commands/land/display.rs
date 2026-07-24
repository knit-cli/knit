//! Human-readable rendering of land plans, run status, and per-repo PR/check state.

use super::{LandPlan, LandRun, LandStep, LandStepKind};
use crate::checkout::checkout_dir;
use crate::model::DeployMode;
use crate::output as out;
use crate::providers::{self, publication_for_repo, CheckRun, PrTarget};
use crate::store::ActiveBundle;
use std::path::Path;

pub(super) fn print_plan(active: &ActiveBundle, plan: &LandPlan, path: &Path) {
    println!("{} {}", out::heading("Land plan"), out::node(&plan.id));
    println!("{} {}", out::heading("Provider:"), plan.provider);
    println!(
        "{} {}",
        out::heading("Plan file:"),
        out::path(path.display())
    );
    print_landing_targets(
        active,
        plan.steps
            .iter()
            .filter(|step| step.step_type == LandStepKind::MergePr)
            .filter_map(|step| step.repo_id.as_ref()),
        plan.target_branch.as_deref(),
        plan.steps
            .iter()
            .any(|step| step.step_type == LandStepKind::Deploy),
    );
    println!();
    for step in &plan.steps {
        println!(
            "{} {} {}",
            out::node(&step.id),
            out::heading(step.step_type.as_str()),
            planned_step_target(step)
        );
        if !step.needs.is_empty() {
            println!("  needs {}", step.needs.join(", "));
        }
    }
    println!();
    match plan.target_branch.as_deref() {
        Some(target) => println!(
            "{} knit land --target {} apply",
            out::heading("Apply:"),
            target
        ),
        None => println!("{} knit land apply", out::heading("Apply:")),
    }
}

pub(super) fn print_run_status(active: &ActiveBundle, run: &LandRun, path: &Path) {
    println!(
        "{} {} {}",
        out::heading("Land run"),
        out::node(&run.id),
        out::status(run.status.as_str())
    );
    println!(
        "{} {}",
        out::heading("Run file:"),
        out::path(path.display())
    );
    print_landing_targets(
        active,
        run.steps
            .iter()
            .filter(|step| step.step_type == LandStepKind::MergePr)
            .filter_map(|step| step.repo_id.as_ref()),
        None,
        run.steps
            .iter()
            .any(|step| step.step_type == LandStepKind::Deploy),
    );
    println!();
    for step in &run.steps {
        println!(
            "{} {} {}",
            out::node(&step.id),
            out::status(&format!("{:<9}", step.status)),
            step.detail.as_deref().unwrap_or("")
        );
        if let Some(repo_id) = &step.repo_id {
            print_pr_status(active, repo_id, step.publication_url.as_deref());
        }
        if let Some(stderr) = &step.stderr {
            if !stderr.trim().is_empty() {
                println!("  {}", out::danger(stderr.trim()));
            }
        }
    }
}

pub(super) fn print_planned_step(active: &ActiveBundle, step: &LandStep) {
    println!(
        "{} {} {}",
        out::node(&step.id),
        out::muted("planned"),
        step.step_type
    );
    if let Some(repo_id) = &step.repo_id {
        print_pr_status(active, repo_id, None);
    }
}

pub(super) fn print_plan_landing_targets(active: &ActiveBundle, plan: &LandPlan) {
    print_landing_targets(
        active,
        plan.steps
            .iter()
            .filter(|step| step.step_type == LandStepKind::MergePr)
            .filter_map(|step| step.repo_id.as_ref()),
        plan.target_branch.as_deref(),
        plan.steps
            .iter()
            .any(|step| step.step_type == LandStepKind::Deploy),
    );
}

fn print_landing_targets<'a>(
    active: &ActiveBundle,
    repo_ids: impl IntoIterator<Item = &'a String>,
    explicit_target: Option<&str>,
    has_deployment_steps: bool,
) {
    let targets = repo_ids
        .into_iter()
        .filter_map(|repo_id| {
            let publication = publication_for_repo(&active.bundle, repo_id)?;
            let base_branch = explicit_target.unwrap_or(&publication.base_branch);
            let is_alternate = active
                .bundle
                .repos
                .iter()
                .find(|repo| repo.id == *repo_id)
                .is_some_and(|repo| base_branch != repo.base_branch);
            Some((repo_id, base_branch, is_alternate))
        })
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return;
    }

    println!("{}", out::heading("Lands into:"));
    let has_alternate_target = targets.iter().any(|(_, _, is_alternate)| *is_alternate);
    for (repo_id, base_branch, _) in targets {
        println!("  {} -> {}", out::repo(repo_id), out::branch(base_branch));
    }
    if has_alternate_target {
        if has_deployment_steps {
            println!(
                "{} matching `landing.targets.<branch>` deployment steps are included below.",
                out::heading("Deployment:")
            );
        } else {
            println!(
                "{} no deployment steps matched these branches; declare them under `landing.targets.<branch>.deployments` or add an explicit step to this plan.",
                out::heading("Deployment:")
            );
        }
    }
}

fn planned_step_target(step: &LandStep) -> String {
    match step.step_type {
        LandStepKind::Deploy => {
            let repo = step
                .repo_id
                .as_deref()
                .map(out::repo)
                .unwrap_or_else(|| out::muted("workspace"));
            let mode = step.deployment_mode.unwrap_or(if step.command.is_empty() {
                DeployMode::Push
            } else {
                DeployMode::Command
            });
            format!("{repo} {mode}")
        }
        _ => step
            .repo_id
            .as_deref()
            .map(out::repo)
            .unwrap_or_else(|| step.command.join(" ")),
    }
}

fn print_pr_status(active: &ActiveBundle, repo_id: &str, fallback_publication_url: Option<&str>) {
    let Some(repo) = active.bundle.repos.iter().find(|repo| repo.id == repo_id) else {
        println!(
            "  {} {}",
            out::repo(repo_id),
            out::danger("repo not tracked")
        );
        return;
    };
    let Some(cwd) = checkout_dir(active, repo) else {
        println!(
            "  {} {}",
            out::repo(repo_id),
            out::danger("checkout missing")
        );
        return;
    };
    let publication_url = publication_for_repo(&active.bundle, repo_id)
        .map(|publication| publication.url.as_str())
        .or(fallback_publication_url);
    let Some(publication_url) = publication_url else {
        println!(
            "  {} {}",
            out::repo(repo_id),
            out::danger("publication missing")
        );
        return;
    };
    let forge = match providers::for_repo(repo) {
        Ok(forge) => forge,
        Err(error) => {
            println!(
                "  {} {} {}",
                out::repo(repo_id),
                out::danger("provider unavailable:"),
                error
            );
            return;
        }
    };
    let target = PrTarget::checkout(&cwd);
    match forge.view(&target, publication_url) {
        Ok(pr) => {
            println!(
                "  {} #{} {} {}",
                out::repo(repo_id),
                pr.number,
                out::status(pr.state.as_deref().unwrap_or("UNKNOWN")),
                pr.url
            );
            match forge.check_runs(&target, publication_url, true) {
                Ok(runs) => println!("    checks {}", check_status_label(&runs)),
                Err(error) => println!("    {} {}", out::danger("checks unavailable:"), error),
            }
        }
        Err(error) => println!(
            "  {} {} {}",
            out::repo(repo_id),
            out::danger("PR status unavailable:"),
            error
        ),
    }
}

fn check_status_label(runs: &[CheckRun]) -> String {
    if runs.is_empty() {
        return out::ok("passed (no required checks)");
    }
    let failed = runs.iter().filter(|run| {
        matches!(run.bucket.as_deref(), Some("fail" | "cancel"))
            || matches!(run.state.as_deref(), Some("FAILURE" | "CANCELLED"))
    });
    let failed = failed.map(|run| run.name.as_str()).collect::<Vec<_>>();
    if !failed.is_empty() {
        return out::danger(format!("failed ({})", failed.join(", ")));
    }
    let pending = runs.iter().any(|run| {
        !matches!(run.bucket.as_deref(), Some("pass" | "skipping"))
            && !matches!(run.state.as_deref(), Some("SUCCESS" | "SKIPPED"))
    });
    if pending {
        out::warn("pending")
    } else {
        out::ok("passed")
    }
}
