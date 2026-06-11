//! Executes a land plan: schedules steps into dependency waves, runs each step
//! (merge PR, wait for checks, run command, deploy), and records progress into
//! the land run file and bundle ledger.

use super::{
    display_path_for_storage, ensure_open_and_ready, non_empty, repo_context, required_repo_id,
    resolve_stored_path, resolve_subdir, run_step, safe_timestamp, state_is_merged, LandCheckout,
    LandPlan, LandRun, LandRunStep, LandStatus, LandStep, LandStepKind, PublicationUpdate,
    StepOutcome, LAND_RUN_KIND,
};
use crate::git::{git_output, is_git_worktree};
use crate::ids::{node_id, slugify};
use crate::model::{BundleNode, DeployCheckoutUpdate, DeployMode, PublicationEntry, SCHEMA_VERSION};
use crate::output as out;
use crate::providers::{self, publication_for_repo, PrTarget, PullRequest};
use crate::store::{save_active_bundle, write_json, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) fn execute_run(
    active: &mut ActiveBundle,
    plan: &LandPlan,
    order: &[String],
    run: &mut LandRun,
    run_path: &Path,
) -> Result<()> {
    let waves = step_waves(&plan.steps, order)?;

    for wave in &waves {
        let mut pending: Vec<(&LandStep, usize)> = Vec::new();
        for step_id in wave {
            let step = plan
                .steps
                .iter()
                .find(|s| &s.id == step_id)
                .expect("validated plan order references a real step");
            let run_index = run
                .steps
                .iter()
                .position(|run_step| run_step.id == step.id)
                .expect("run contains every plan step");
            if run.steps[run_index].status == LandStatus::Succeeded {
                continue;
            }
            ensure_needs_succeeded(run, step)?;
            run.steps[run_index].status = LandStatus::Running;
            run.steps[run_index].started_at = Some(now_iso());
            run.steps[run_index].finished_at = None;
            run.steps[run_index].detail = None;
            run.steps[run_index].stdout = None;
            run.steps[run_index].stderr = None;
            run.steps[run_index].exit_code = None;
            pending.push((step, run_index));
        }

        if pending.is_empty() {
            continue;
        }

        run.status = LandStatus::Running;
        run.updated_at = now_iso();
        write_json(run_path, run)?;

        for (step, _) in &pending {
            println!("{} {}", out::movement("running"), out::node(&step.id));
        }

        let active_shared: &ActiveBundle = active;
        let results: Vec<(String, StepOutcome)> = if pending.len() == 1 {
            let (step, _) = &pending[0];
            let outcome = match execute_step(active_shared, plan, step) {
                Ok(outcome) => outcome,
                Err(error) => StepOutcome {
                    success: false,
                    detail: error.to_string(),
                    publication_url: step_publication(active_shared, step)
                        .map(|publication| publication.url),
                    stdout: None,
                    stderr: None,
                    exit_code: None,
                    publication_update: None,
                },
            };
            vec![(step.id.clone(), outcome)]
        } else {
            let mut results = Vec::with_capacity(pending.len());
            std::thread::scope(|scope| {
                let handles: Vec<_> = pending
                    .iter()
                    .map(|(step, _)| {
                        let step_id = step.id.clone();
                        scope.spawn(move || {
                            let outcome = match execute_step(active_shared, plan, step) {
                                Ok(outcome) => outcome,
                                Err(error) => StepOutcome {
                                    success: false,
                                    detail: error.to_string(),
                                    publication_url: step_publication(active_shared, step)
                                        .map(|publication| publication.url),
                                    stdout: None,
                                    stderr: None,
                                    exit_code: None,
                                    publication_update: None,
                                },
                            };
                            (step_id, outcome)
                        })
                    })
                    .collect();
                for handle in handles {
                    results.push(handle.join().expect("land step thread panicked"));
                }
            });
            results
        };

        let mut bundle_dirty = false;
        let mut any_failed = false;
        for (step_id, outcome) in &results {
            let run_index = run
                .steps
                .iter()
                .position(|run_step| &run_step.id == step_id)
                .expect("run contains every plan step");
            let run_step = &mut run.steps[run_index];
            run_step.status = if outcome.success {
                LandStatus::Succeeded
            } else {
                LandStatus::Failed
            };
            run_step.finished_at = Some(now_iso());
            run_step.detail = Some(outcome.detail.clone());
            run_step.publication_url = outcome.publication_url.clone();
            run_step.stdout = outcome.stdout.clone();
            run_step.stderr = outcome.stderr.clone();
            run_step.exit_code = outcome.exit_code;

            if outcome.success {
                println!(
                    "{} {} {}",
                    out::ok("succeeded"),
                    out::node(step_id),
                    out::muted(&outcome.detail)
                );
            } else {
                any_failed = true;
                println!(
                    "{} {} {}",
                    out::danger("failed"),
                    out::node(step_id),
                    outcome.detail
                );
            }

            if let Some(update) = &outcome.publication_update {
                let forge = providers::for_repo(&update.repo)?;
                providers::upsert_publication(
                    &mut active.bundle,
                    &update.repo,
                    forge.as_ref(),
                    &update.pr,
                );
                bundle_dirty = true;
            }
        }

        run.updated_at = now_iso();
        run.status = if any_failed {
            LandStatus::Failed
        } else {
            LandStatus::Running
        };
        write_json(run_path, run)?;
        if bundle_dirty {
            save_active_bundle(active)?;
        }

        if any_failed {
            let failed_ids: Vec<_> = results
                .iter()
                .filter(|(_, o)| !o.success)
                .map(|(id, _)| id.as_str())
                .collect();
            let label = if failed_ids.len() == 1 { "step" } else { "steps" };
            bail!(
                "Land run {} stopped at {} {}. Fix the issue and run `knit land resume`.",
                run.id,
                label,
                failed_ids.join(", ")
            );
        }
    }

    run.status = LandStatus::Succeeded;
    run.updated_at = now_iso();
    write_json(run_path, run)?;
    append_landed_node(active, plan, run)?;
    save_active_bundle(active)?;
    println!("{} {}", out::heading("Feature landed"), out::node(&run.id));
    Ok(())
}

pub(super) fn step_waves(steps: &[LandStep], order: &[String]) -> Result<Vec<Vec<String>>> {
    let mut waves = Vec::new();
    let mut satisfied = BTreeSet::new();
    let mut remaining: BTreeSet<String> = order.iter().cloned().collect();

    while !remaining.is_empty() {
        let mut wave = Vec::new();
        for step_id in order {
            if !remaining.contains(step_id) {
                continue;
            }
            let step = steps
                .iter()
                .find(|s| &s.id == step_id)
                .expect("order references a real step");
            if step.needs.iter().all(|need| satisfied.contains(need)) {
                wave.push(step_id.clone());
            }
        }
        if wave.is_empty() {
            bail!("land plan contains a dependency cycle among remaining steps");
        }
        for id in &wave {
            remaining.remove(id);
            satisfied.insert(id.clone());
        }
        waves.push(wave);
    }

    Ok(waves)
}

fn execute_step(active: &ActiveBundle, plan: &LandPlan, step: &LandStep) -> Result<StepOutcome> {
    match step.step_type {
        LandStepKind::MergePr => execute_merge_pr(active, plan, step),
        LandStepKind::WaitChecks => execute_wait_checks(active, step),
        LandStepKind::Run => execute_run_command(active, step),
        LandStepKind::Deploy => execute_deployment(active, step),
    }
}

fn execute_merge_pr(active: &ActiveBundle, plan: &LandPlan, step: &LandStep) -> Result<StepOutcome> {
    let repo_id = required_repo_id(step)?;
    let (_, repo, cwd) = repo_context(active, repo_id)?;
    let forge = providers::for_repo(&repo)?;
    let target = PrTarget::checkout(&cwd);
    let publication = publication_for_repo(&active.bundle, repo_id)
        .with_context(|| format!("{repo_id}: no review publication recorded"))?
        .clone();
    let pr = forge.view(&target, &publication.url)?;
    if state_is_merged(&pr) {
        return Ok(StepOutcome {
            success: true,
            detail: "already merged".to_string(),
            publication_url: Some(publication.url),
            stdout: None,
            stderr: None,
            exit_code: None,
            publication_update: Some(PublicationUpdate { repo, pr }),
        });
    }
    ensure_open_and_ready(repo_id, &pr)?;

    let mut detail = Vec::new();
    if step.wait_for_checks.unwrap_or(true) {
        let summary = forge.wait_for_checks(
            &target,
            &publication.url,
            step.required_checks_only.unwrap_or(true),
            step.timeout_seconds.unwrap_or(1800),
            step.interval_seconds.unwrap_or(10),
        )?;
        detail.push(format!("checks {}", summary.status));
    }

    let method = step.method.unwrap_or_default();
    forge.merge(
        &target,
        &publication.url,
        method.as_str(),
        step.delete_branch.unwrap_or(false),
        pr.head_ref_oid.as_deref(),
    )?;
    let refreshed = forge.view(&target, &publication.url).unwrap_or_else(|_| PullRequest {
        state: Some("MERGED".to_string()),
        ..pr.clone()
    });
    detail.push(format!("merged with {method}"));
    let _ = plan;

    Ok(StepOutcome {
        success: true,
        detail: detail.join("; "),
        publication_url: Some(publication.url),
        stdout: None,
        stderr: None,
        exit_code: None,
        publication_update: Some(PublicationUpdate {
            repo,
            pr: refreshed,
        }),
    })
}

fn execute_wait_checks(active: &ActiveBundle, step: &LandStep) -> Result<StepOutcome> {
    let repo_id = required_repo_id(step)?;
    let (_, repo, cwd) = repo_context(active, repo_id)?;
    let forge = providers::for_repo(&repo)?;
    let target = PrTarget::checkout(&cwd);
    let publication = publication_for_repo(&active.bundle, repo_id)
        .with_context(|| format!("{repo_id}: no review publication recorded"))?;
    let summary = forge.wait_for_checks(
        &target,
        &publication.url,
        step.required_only.unwrap_or(true),
        step.timeout_seconds.unwrap_or(1800),
        step.interval_seconds.unwrap_or(10),
    )?;
    let detail = if summary.runs.is_empty() {
        summary.status
    } else {
        format!("{} ({} check(s))", summary.status, summary.runs.len())
    };

    Ok(StepOutcome {
        success: true,
        detail,
        publication_url: Some(publication.url.clone()),
        stdout: None,
        stderr: None,
        exit_code: None,
        publication_update: None,
    })
}

fn execute_run_command(active: &ActiveBundle, step: &LandStep) -> Result<StepOutcome> {
    if step.command.is_empty() {
        bail!("run step `{}` has an empty command", step.id);
    }
    let cwd = step
        .cwd
        .as_deref()
        .map(|cwd| resolve_stored_path(&active.root, cwd))
        .unwrap_or_else(|| active.root.clone());
    let output = Command::new(&step.command[0])
        .args(&step.command[1..])
        .current_dir(&cwd)
        .envs(&step.env)
        .output()
        .with_context(|| {
            format!(
                "failed to run `{}` in {}",
                step.command.join(" "),
                cwd.display()
            )
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code();
    let success = output.status.success();
    let detail = if success {
        format!("ran `{}`", step.command.join(" "))
    } else {
        format!(
            "`{}` exited with {}",
            step.command.join(" "),
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )
    };

    Ok(StepOutcome {
        success,
        detail,
        publication_url: None,
        stdout: non_empty(stdout),
        stderr: non_empty(stderr),
        exit_code,
        publication_update: None,
    })
}

fn execute_deployment(active: &ActiveBundle, step: &LandStep) -> Result<StepOutcome> {
    let mode = step.deployment_mode.unwrap_or(if step.command.is_empty() {
        DeployMode::Push
    } else {
        DeployMode::Command
    });
    let repo_id = required_repo_id(step)?;

    if mode == DeployMode::Push {
        if !step.command.is_empty() {
            bail!(
                "deploy step `{}` uses push mode and must not provide command",
                step.id
            );
        }
        return Ok(StepOutcome {
            success: true,
            detail: format!("{repo_id} deployment triggered by merge"),
            publication_url: None,
            stdout: None,
            stderr: None,
            exit_code: None,
            publication_update: None,
        });
    }

    if step.command.is_empty() {
        bail!(
            "deploy step `{}` must provide command in command mode",
            step.id
        );
    }

    let (cwd, checkout_detail) = deployment_cwd(active, repo_id, step)?;
    let output = Command::new(&step.command[0])
        .args(&step.command[1..])
        .current_dir(&cwd)
        .envs(&step.env)
        .env("KNIT_ROOT", &active.root)
        .env("KNIT_BUNDLE", &active.bundle.id)
        .env("KNIT_REPO", repo_id)
        .env("KNIT_DEPLOY_CHECKOUT", &cwd)
        .output()
        .with_context(|| {
            format!(
                "failed to run deploy `{}` in {}",
                step.command.join(" "),
                cwd.display()
            )
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code();
    let success = output.status.success();
    let detail = if success {
        format!("deployed {repo_id} from {checkout_detail}")
    } else {
        format!(
            "deploy `{}` exited with {}",
            step.command.join(" "),
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )
    };

    Ok(StepOutcome {
        success,
        detail,
        publication_url: None,
        stdout: non_empty(stdout),
        stderr: non_empty(stderr),
        exit_code,
        publication_update: None,
    })
}

fn deployment_cwd(active: &ActiveBundle, repo_id: &str, step: &LandStep) -> Result<(PathBuf, String)> {
    let (base, detail) = if let Some(checkout) = &step.checkout {
        let path = prepare_deployment_checkout(active, repo_id, checkout)?;
        let remote = checkout.remote.as_deref().unwrap_or("origin");
        (path, format!("{remote}/{}", checkout.branch))
    } else {
        let (_, _, cwd) = repo_context(active, repo_id)?;
        (cwd, "bundle checkout".to_string())
    };
    let cwd = step
        .cwd
        .as_deref()
        .map(|cwd| resolve_subdir(&base, cwd))
        .unwrap_or(base);
    if !cwd.exists() {
        bail!(
            "deploy step `{}` cwd does not exist: {}",
            step.id,
            cwd.display()
        );
    }
    Ok((cwd, detail))
}

fn prepare_deployment_checkout(
    active: &ActiveBundle,
    repo_id: &str,
    checkout: &LandCheckout,
) -> Result<PathBuf> {
    if checkout.branch.trim().is_empty() {
        bail!("deploy checkout branch must not be empty");
    }
    let repo = active
        .bundle
        .repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .with_context(|| format!("{repo_id}: repo is not tracked in this bundle"))?;
    let repo_root = PathBuf::from(&repo.path);
    if !repo_root.exists() {
        bail!(
            "{}: original repo path is missing: {}",
            repo_id,
            repo_root.display()
        );
    }

    let remote = checkout.remote.as_deref().unwrap_or("origin");
    let update = checkout.update.unwrap_or_default();
    let path = active
        .root
        .join(".knit/land-worktrees")
        .join(&active.bundle.id)
        .join(repo_id)
        .join(slugify(&checkout.branch));

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let target_ref = if update == DeployCheckoutUpdate::None {
        format!("{remote}/{}", checkout.branch)
    } else {
        fetch_deploy_branch(&repo_root, remote, &checkout.branch)?;
        "FETCH_HEAD".to_string()
    };

    if path.exists() {
        if !is_git_worktree(&path) {
            bail!(
                "{}: deployment checkout path exists but is not a git worktree: {}",
                repo_id,
                path.display()
            );
        }
        ensure_deploy_checkout_clean(repo_id, &path)?;
        git_output(
            &path,
            [
                OsString::from("checkout"),
                OsString::from("--detach"),
                OsString::from(&target_ref),
            ],
        )?;
    } else {
        git_output(
            &repo_root,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                path.as_os_str().to_os_string(),
                OsString::from(&target_ref),
            ],
        )?;
    }

    Ok(path)
}

fn fetch_deploy_branch(repo_root: &Path, remote: &str, branch: &str) -> Result<()> {
    git_output(
        repo_root,
        [
            OsString::from("fetch"),
            OsString::from(remote),
            OsString::from(branch),
        ],
    )?;
    Ok(())
}

fn ensure_deploy_checkout_clean(repo_id: &str, path: &Path) -> Result<()> {
    let status = git_output(path, ["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        bail!(
            "{}: deployment checkout has uncommitted changes in {}",
            repo_id,
            path.display()
        );
    }
    Ok(())
}

pub(super) fn ensure_needs_succeeded(run: &LandRun, step: &LandStep) -> Result<()> {
    for need in &step.needs {
        let Some(run_step) = run_step(run, need) else {
            bail!("run is missing dependency step `{need}`");
        };
        if run_step.status != LandStatus::Succeeded {
            bail!(
                "step `{}` is waiting for `{need}`, which is {}",
                step.id,
                run_step.status
            );
        }
    }
    Ok(())
}

fn append_landed_node(active: &mut ActiveBundle, plan: &LandPlan, run: &LandRun) -> Result<()> {
    if active
        .bundle
        .nodes
        .iter()
        .any(|node| node.run_id.as_deref() == Some(run.id.as_str()))
    {
        return Ok(());
    }

    let repo_ids = plan
        .steps
        .iter()
        .filter_map(|step| step.repo_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let publication_urls = repo_ids
        .iter()
        .filter_map(|repo_id| publication_for_repo(&active.bundle, repo_id))
        .map(|publication| publication.url.clone())
        .chain(
            run.steps
                .iter()
                .filter(|step| step.repo_id.is_some())
                .filter_map(|step| step.publication_url.clone()),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    active.bundle.nodes.push(BundleNode::feature_landed(
        node_id("land"),
        now_iso(),
        plan.id.clone(),
        run.id.clone(),
        plan.provider.clone(),
        repo_ids,
        publication_urls,
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    Ok(())
}

pub(super) fn new_run(active: &ActiveBundle, plan: &LandPlan, plan_path: &Path) -> LandRun {
    let now = now_iso();
    LandRun {
        schema_version: SCHEMA_VERSION.to_string(),
        kind: LAND_RUN_KIND.to_string(),
        id: format!("run-{}", safe_timestamp()),
        plan_id: plan.id.clone(),
        bundle_id: active.bundle.id.clone(),
        provider: plan.provider.clone(),
        plan_path: display_path_for_storage(&active.root, plan_path),
        status: LandStatus::Running,
        created_at: now.clone(),
        updated_at: now,
        steps: plan
            .steps
            .iter()
            .map(|step| LandRunStep {
                id: step.id.clone(),
                step_type: step.step_type,
                status: LandStatus::Pending,
                repo_id: step.repo_id.clone(),
                publication_url: step_publication(active, step).map(|publication| publication.url),
                started_at: None,
                finished_at: None,
                detail: None,
                stdout: None,
                stderr: None,
                exit_code: None,
            })
            .collect(),
    }
}

fn step_publication(active: &ActiveBundle, step: &LandStep) -> Option<PublicationEntry> {
    let repo_id = step.repo_id.as_deref()?;
    publication_for_repo(&active.bundle, repo_id).cloned()
}
