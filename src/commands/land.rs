use crate::advice;
use crate::checkout::{checkout_dir, ensure_expected_branch};
use crate::git::{current_branch, git_output, is_ancestor, rev_list, rev_parse};
use crate::ids::node_id;
use crate::model::{BundleNode, PublicationEntry, RepoChange, RepoEntry, SCHEMA_VERSION};
use crate::output as out;
use crate::providers::github::{self, publication_for_repo, PullRequest};
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, read_json, save_active_bundle, write_json,
    ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const LAND_PLAN_KIND: &str = "KnitLandPlan";
const LAND_RUN_KIND: &str = "KnitLandRun";
const STEP_MERGE_PR: &str = "merge_pr";
const STEP_WAIT_CHECKS: &str = "wait_checks";
const STEP_RUN: &str = "run";
const STATUS_PENDING: &str = "pending";
const STATUS_RUNNING: &str = "running";
const STATUS_SUCCEEDED: &str = "succeeded";
const STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LandPlan {
    schema_version: String,
    kind: String,
    id: String,
    provider: String,
    bundle_id: String,
    created_at: String,
    steps: Vec<LandStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LandStep {
    id: String,
    #[serde(rename = "type")]
    step_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    needs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wait_for_checks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    required_checks_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delete_branch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    required_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interval_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LandRun {
    schema_version: String,
    kind: String,
    id: String,
    plan_id: String,
    bundle_id: String,
    provider: String,
    plan_path: String,
    status: String,
    created_at: String,
    updated_at: String,
    steps: Vec<LandRunStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LandRunStep {
    id: String,
    #[serde(rename = "type")]
    step_type: String,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

#[derive(Debug)]
struct StepOutcome {
    success: bool,
    detail: String,
    publication_url: Option<String>,
    stdout: Option<String>,
    stderr: Option<String>,
    exit_code: Option<i32>,
}

pub fn generate_land_plan(provider: &str, out_path: Option<&Path>, force: bool) -> Result<()> {
    ensure_provider(provider)?;
    let active = load_active_bundle()?;
    let plan = build_default_plan(&active, provider)?;
    let path = out_path
        .map(resolve_user_path)
        .unwrap_or_else(|| default_plan_path(&active));
    if path.exists() && !force {
        bail!(
            "Land plan already exists at {}. Pass --force to replace it.",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_json(&path, &plan)?;
    print_plan(&plan, &path);
    advice::print(
        &active.root,
        "inspect or edit this plan, then run `knit land apply` when you are ready to execute it.",
    );
    Ok(())
}

pub fn land_default() -> Result<()> {
    let active = load_active_bundle()?;
    if let Some(path) = resolve_land_run_path(&active, None)? {
        let run: LandRun = read_json(&path)?;
        print_run_status(&active, &run, &path);
        if run.status == STATUS_SUCCEEDED {
            return Ok(());
        }
        advice::print(
            &active.root,
            "fix the failed or incomplete step, then run `knit land resume` when you are ready to continue execution.",
        );
        return Ok(());
    }

    let plan_path = default_plan_path(&active);
    if plan_path.exists() {
        let plan: LandPlan = read_json(&plan_path)?;
        validate_plan_for_bundle(&active, &plan)?;
        print_plan(&plan, &plan_path);
        advice::print(
            &active.root,
            "inspect or edit this plan, then run `knit land apply` when you are ready to execute it.",
        );
        return Ok(());
    }

    drop(active);
    generate_land_plan("github", None, false)
}

pub fn apply_land_plan(plan_path: Option<&Path>) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let path = resolve_land_plan_path(&active, plan_path)?;
    if !path.exists() {
        bail!(
            "No land plan found at {}. Run `knit land plan` first, inspect the plan, then run `knit land apply`.",
            path.display()
        );
    }
    let plan: LandPlan = read_json(&path)?;
    validate_plan_for_bundle(&active, &plan)?;
    let order = ordered_step_ids(&plan.steps)?;
    preflight_publications(&active, &plan, None)?;

    let run_path = new_run_path(&active, &plan);
    if let Some(parent) = run_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut run = new_run(&active, &plan, &path);
    write_json(&run_path, &run)?;
    execute_run(&mut active, &plan, &order, &mut run, &run_path)
}

pub fn resume_land_run(run_path: Option<&Path>) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let path = resolve_land_run_path(&active, run_path)?
        .with_context(|| "No land run found. Run `knit land apply` first.")?;
    let mut run: LandRun = read_json(&path)?;
    if run.status == STATUS_SUCCEEDED {
        println!(
            "{} {} is already succeeded.",
            out::heading("Land run"),
            out::node(&run.id)
        );
        return Ok(());
    }
    let plan_path = resolve_stored_path(&active.root, &run.plan_path);
    let plan: LandPlan = read_json(&plan_path)?;
    validate_plan_for_bundle(&active, &plan)?;
    let order = ordered_step_ids(&plan.steps)?;
    preflight_publications(&active, &plan, Some(&run))?;
    run.status = STATUS_RUNNING.to_string();
    run.updated_at = now_iso();
    write_json(&path, &run)?;
    execute_run(&mut active, &plan, &order, &mut run, &path)
}

pub fn show_land_status(run_path: Option<&Path>) -> Result<()> {
    let active = load_active_bundle()?;
    if let Some(path) = resolve_land_run_path(&active, run_path)? {
        let run: LandRun = read_json(&path)?;
        print_run_status(&active, &run, &path);
        return Ok(());
    }

    let plan_path = default_plan_path(&active);
    if !plan_path.exists() {
        bail!("No land run or default land plan found. Run `knit land plan` first.");
    }
    let plan: LandPlan = read_json(&plan_path)?;
    validate_plan_for_bundle(&active, &plan)?;
    println!(
        "{} {}",
        out::heading("Land plan:"),
        out::path(plan_path.display())
    );
    if plan.provider == "github" {
        println!(
            "{} each recorded GitHub PR base branch",
            out::heading("Lands into:")
        );
    }
    for step in &plan.steps {
        print_planned_step(&active, step);
    }
    Ok(())
}

pub fn update_land_branches(
    selectors: &[String],
    all: bool,
    push: bool,
    set_upstream: bool,
    continue_merge: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }
    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let targets = indexes
        .iter()
        .map(|index| update_target(&active, *index))
        .collect::<Result<Vec<_>>>()?;

    if !continue_merge {
        preflight_update_targets(&targets)?;
    } else {
        preflight_continue_targets(&targets)?;
    }

    let mut changes = Vec::new();
    if continue_merge {
        for target in &targets {
            if let Some(change) = record_existing_update(&mut active, target)? {
                print_update_change(&change, "recorded");
                changes.push(change);
            } else {
                println!(
                    "{}: {}",
                    out::repo(&target.repo_id),
                    out::muted("unchanged")
                );
            }
        }
    } else {
        for target in &targets {
            match merge_base_into_feature(&mut active, target) {
                Ok(Some(change)) => {
                    print_update_change(&change, "updated");
                    changes.push(change);
                }
                Ok(None) => {
                    println!(
                        "{}: {}",
                        out::repo(&target.repo_id),
                        out::muted("already contains latest base")
                    );
                }
                Err(error) => {
                    bail!(
                        "{}: failed to update from base: {error:#}\nResolve the merge in {}, commit it, then run `knit land update --continue-merge{}`.",
                        target.repo_id,
                        target.cwd.display(),
                        if push { " --push" } else { "" }
                    );
                }
            }
        }
    }

    if !changes.is_empty() {
        append_land_update_node(&mut active, changes)?;
        save_active_bundle(&active)?;
    } else {
        println!("{}", out::ok("No feature branches needed base updates."));
    }

    if push {
        push_update_targets(&targets, set_upstream)?;
        refresh_update_publications(&mut active, &targets)?;
        save_active_bundle(&active)?;
    }

    Ok(())
}

fn build_default_plan(active: &ActiveBundle, provider: &str) -> Result<LandPlan> {
    let mut steps = Vec::new();
    let mut previous: Option<String> = None;
    for repo in &active.bundle.repos {
        if publication_for_repo(&active.bundle, &repo.id).is_none() {
            continue;
        }
        let id = format!("merge-{}", repo.id);
        let needs = previous.iter().cloned().collect::<Vec<_>>();
        steps.push(LandStep {
            id: id.clone(),
            step_type: STEP_MERGE_PR.to_string(),
            needs,
            repo_id: Some(repo.id.clone()),
            method: Some("squash".to_string()),
            wait_for_checks: Some(true),
            required_checks_only: Some(true),
            delete_branch: Some(false),
            required_only: None,
            timeout_seconds: Some(1800),
            interval_seconds: Some(10),
            cwd: None,
            command: Vec::new(),
            env: BTreeMap::new(),
        });
        previous = Some(id);
    }

    if steps.is_empty() {
        bail!(
            "No GitHub PR publications are recorded for this bundle. Run `knit publish github create` first."
        );
    }

    Ok(LandPlan {
        schema_version: SCHEMA_VERSION.to_string(),
        kind: LAND_PLAN_KIND.to_string(),
        id: format!("land-{}", active.bundle.id),
        provider: provider.to_string(),
        bundle_id: active.bundle.id.clone(),
        created_at: now_iso(),
        steps,
    })
}

struct LandUpdateTarget {
    repo_index: usize,
    repo_id: String,
    cwd: PathBuf,
    feature_branch: String,
    base_branch: String,
    publication_url: String,
    recorded_head: String,
}

fn update_target(active: &ActiveBundle, repo_index: usize) -> Result<LandUpdateTarget> {
    let repo = &active.bundle.repos[repo_index];
    let publication = publication_for_repo(&active.bundle, &repo.id).with_context(|| {
        format!(
            "{}: no GitHub PR publication recorded. Run `knit publish github create` first.",
            repo.id
        )
    })?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    let feature_branch = repo.feature_branch.clone().with_context(|| {
        format!(
            "{}: no feature branch recorded. Run `knit worktree`.",
            repo.id
        )
    })?;
    let recorded_head = repo.head_sha.clone().with_context(|| {
        format!(
            "{}: no recorded feature head. Run `knit sync` before updating.",
            repo.id
        )
    })?;

    Ok(LandUpdateTarget {
        repo_index,
        repo_id: repo.id.clone(),
        cwd,
        feature_branch,
        base_branch: publication.base_branch.clone(),
        publication_url: publication.url.clone(),
        recorded_head,
    })
}

fn preflight_update_targets(targets: &[LandUpdateTarget]) -> Result<()> {
    for target in targets {
        ensure_update_branch(target)?;
        ensure_clean_worktree(target)?;
        let actual_head = rev_parse(&target.cwd, "HEAD")
            .with_context(|| format!("{}: failed to read HEAD", target.repo_id))?;
        if actual_head != target.recorded_head {
            bail!(
                "{}: feature checkout is at {}, but the bundle records {}. Run `knit sync` first, or use `knit land update --continue-merge` after resolving an update merge.",
                target.repo_id,
                out::sha(crate::ids::short_sha(&actual_head)),
                out::sha(crate::ids::short_sha(&target.recorded_head))
            );
        }
    }
    Ok(())
}

fn preflight_continue_targets(targets: &[LandUpdateTarget]) -> Result<()> {
    for target in targets {
        ensure_update_branch(target)?;
        ensure_clean_worktree(target)?;
    }
    Ok(())
}

fn ensure_update_branch(target: &LandUpdateTarget) -> Result<()> {
    let actual = current_branch(&target.cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != target.feature_branch {
        bail!(
            "{}: expected feature branch `{}`, found `{actual}` in {}.",
            target.repo_id,
            target.feature_branch,
            target.cwd.display()
        );
    }
    Ok(())
}

fn ensure_clean_worktree(target: &LandUpdateTarget) -> Result<()> {
    let status = git_output(&target.cwd, ["status", "--short"])?;
    if !status.trim().is_empty() {
        bail!(
            "{}: feature checkout has uncommitted changes in {}. Commit or clean them before updating.",
            target.repo_id,
            target.cwd.display()
        );
    }
    Ok(())
}

fn merge_base_into_feature(
    active: &mut ActiveBundle,
    target: &LandUpdateTarget,
) -> Result<Option<RepoChange>> {
    git_output(
        &target.cwd,
        [
            OsString::from("fetch"),
            OsString::from("origin"),
            OsString::from(&target.base_branch),
        ],
    )
    .with_context(|| {
        format!(
            "{}: failed to fetch origin/{}",
            target.repo_id, target.base_branch
        )
    })?;
    let base_sha = rev_parse(&target.cwd, "FETCH_HEAD")
        .with_context(|| format!("{}: failed to read fetched base head", target.repo_id))?;
    let before = rev_parse(&target.cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature head", target.repo_id))?;

    if is_ancestor(&target.cwd, &base_sha, &before) {
        return Ok(None);
    }

    let base_label = format!("origin/{}", target.base_branch);
    git_output(
        &target.cwd,
        [
            OsString::from("merge"),
            OsString::from("--no-ff"),
            OsString::from("--no-edit"),
            OsString::from(&base_label),
        ],
    )
    .with_context(|| format!("{}: git merge {base_label} failed", target.repo_id))?;

    let after = rev_parse(&target.cwd, "HEAD")
        .with_context(|| format!("{}: failed to read updated feature head", target.repo_id))?;
    let change = advanced_change(&target.cwd, target.repo_id.clone(), before, after)?;
    active.bundle.repos[target.repo_index].head_sha = Some(change.after_sha.clone());
    Ok(Some(change))
}

fn record_existing_update(
    active: &mut ActiveBundle,
    target: &LandUpdateTarget,
) -> Result<Option<RepoChange>> {
    let after = rev_parse(&target.cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature head", target.repo_id))?;
    if after == target.recorded_head {
        return Ok(None);
    }
    let change = advanced_change(
        &target.cwd,
        target.repo_id.clone(),
        target.recorded_head.clone(),
        after,
    )?;
    active.bundle.repos[target.repo_index].head_sha = Some(change.after_sha.clone());
    Ok(Some(change))
}

fn advanced_change(
    cwd: &Path,
    repo_id: String,
    before_sha: String,
    after_sha: String,
) -> Result<RepoChange> {
    if !is_ancestor(cwd, &before_sha, &after_sha) {
        bail!(
            "{repo_id}: update moved the branch in a non-forward direction from {} to {}",
            crate::ids::short_sha(&before_sha),
            crate::ids::short_sha(&after_sha)
        );
    }
    Ok(RepoChange {
        repo_id,
        movement: "advanced".to_string(),
        before_sha: Some(before_sha.clone()),
        after_sha: after_sha.clone(),
        commits: rev_list(cwd, &before_sha, &after_sha).context("failed to list update commits")?,
        dropped_commits: Vec::new(),
    })
}

fn append_land_update_node(active: &mut ActiveBundle, changes: Vec<RepoChange>) -> Result<()> {
    let now = now_iso();
    active.bundle.nodes.push(BundleNode::land_update(
        node_id("land_update"),
        now.clone(),
        github::PROVIDER.to_string(),
        changes,
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now;
    Ok(())
}

fn push_update_targets(targets: &[LandUpdateTarget], set_upstream: bool) -> Result<()> {
    let mut failures = Vec::new();
    for target in targets {
        if let Err(error) = push_update_target(target, set_upstream) {
            println!(
                "{}: {}",
                out::repo(&target.repo_id),
                out::danger("push failed")
            );
            failures.push(format!("{}: {error:#}", target.repo_id));
        }
    }

    if !failures.is_empty() {
        bail!("base update push failed:\n{}", failures.join("\n"));
    }
    Ok(())
}

fn push_update_target(target: &LandUpdateTarget, set_upstream: bool) -> Result<()> {
    let mut args = vec![OsString::from("push")];
    if set_upstream {
        args.push(OsString::from("--set-upstream"));
    }
    args.push(OsString::from("origin"));
    args.push(OsString::from(&target.feature_branch));
    git_output(&target.cwd, args)?;
    let sha = rev_parse(&target.cwd, "HEAD")?;
    println!(
        "{}: {} {} {}",
        out::repo(&target.repo_id),
        out::movement("pushed"),
        out::branch(format!("origin/{}", target.feature_branch)),
        out::sha(crate::ids::short_sha(&sha))
    );
    Ok(())
}

fn refresh_update_publications(
    active: &mut ActiveBundle,
    targets: &[LandUpdateTarget],
) -> Result<()> {
    for target in targets {
        let pr = github::view_pr(&target.cwd, &target.publication_url)
            .with_context(|| format!("{}: failed to refresh PR metadata", target.repo_id))?;
        let repo = active.bundle.repos[target.repo_index].clone();
        github::upsert_publication(&mut active.bundle, &repo, &pr);
    }
    Ok(())
}

fn print_update_change(change: &RepoChange, verb: &str) {
    println!(
        "{}: {} {} -> {} ({} commit(s))",
        out::repo(&change.repo_id),
        out::movement(verb),
        change
            .before_sha
            .as_deref()
            .map(crate::ids::short_sha)
            .map(out::sha)
            .unwrap_or_else(|| out::muted("-")),
        out::sha(crate::ids::short_sha(&change.after_sha)),
        change.commits.len()
    );
}

fn execute_run(
    active: &mut ActiveBundle,
    plan: &LandPlan,
    order: &[String],
    run: &mut LandRun,
    run_path: &Path,
) -> Result<()> {
    for step_id in order {
        let step = plan
            .steps
            .iter()
            .find(|step| &step.id == step_id)
            .expect("validated plan order references a real step");
        let run_index = run
            .steps
            .iter()
            .position(|run_step| run_step.id == step.id)
            .expect("run contains every plan step");

        if run.steps[run_index].status == STATUS_SUCCEEDED {
            continue;
        }
        ensure_needs_succeeded(run, step)?;
        run.steps[run_index].status = STATUS_RUNNING.to_string();
        run.steps[run_index].started_at = Some(now_iso());
        run.steps[run_index].finished_at = None;
        run.steps[run_index].detail = None;
        run.steps[run_index].stdout = None;
        run.steps[run_index].stderr = None;
        run.steps[run_index].exit_code = None;
        run.status = STATUS_RUNNING.to_string();
        run.updated_at = now_iso();
        write_json(run_path, run)?;

        println!("{} {}", out::movement("running"), out::node(&step.id));
        let outcome = match execute_step(active, plan, step) {
            Ok(outcome) => outcome,
            Err(error) => StepOutcome {
                success: false,
                detail: error.to_string(),
                publication_url: step_publication(active, step).map(|publication| publication.url),
                stdout: None,
                stderr: None,
                exit_code: None,
            },
        };

        let run_step = &mut run.steps[run_index];
        run_step.status = if outcome.success {
            STATUS_SUCCEEDED.to_string()
        } else {
            STATUS_FAILED.to_string()
        };
        run_step.finished_at = Some(now_iso());
        run_step.detail = Some(outcome.detail.clone());
        run_step.publication_url = outcome.publication_url;
        run_step.stdout = outcome.stdout;
        run_step.stderr = outcome.stderr;
        run_step.exit_code = outcome.exit_code;
        run.updated_at = now_iso();
        run.status = if outcome.success {
            STATUS_RUNNING.to_string()
        } else {
            STATUS_FAILED.to_string()
        };
        write_json(run_path, run)?;
        save_active_bundle(active)?;

        if outcome.success {
            println!(
                "{} {} {}",
                out::ok("succeeded"),
                out::node(&step.id),
                out::muted(&outcome.detail)
            );
        } else {
            println!(
                "{} {} {}",
                out::danger("failed"),
                out::node(&step.id),
                outcome.detail
            );
            bail!(
                "Land run {} stopped at step {}. Fix the issue and run `knit land resume`.",
                run.id,
                step.id
            );
        }
    }

    run.status = STATUS_SUCCEEDED.to_string();
    run.updated_at = now_iso();
    write_json(run_path, run)?;
    append_landed_node(active, plan, run)?;
    save_active_bundle(active)?;
    println!("{} {}", out::heading("Feature landed"), out::node(&run.id));
    Ok(())
}

fn execute_step(
    active: &mut ActiveBundle,
    plan: &LandPlan,
    step: &LandStep,
) -> Result<StepOutcome> {
    match step.step_type.as_str() {
        STEP_MERGE_PR => execute_merge_pr(active, plan, step),
        STEP_WAIT_CHECKS => execute_wait_checks(active, step),
        STEP_RUN => execute_run_command(active, step),
        step_type => bail!("unknown land step type `{step_type}`"),
    }
}

fn execute_merge_pr(
    active: &mut ActiveBundle,
    plan: &LandPlan,
    step: &LandStep,
) -> Result<StepOutcome> {
    let repo_id = required_repo_id(step)?;
    let (_, repo, cwd) = repo_context(active, repo_id)?;
    let publication = publication_for_repo(&active.bundle, repo_id)
        .with_context(|| format!("{repo_id}: no GitHub PR publication recorded"))?
        .clone();
    let pr = github::view_pr(&cwd, &publication.url)?;
    if state_is_merged(&pr) {
        github::upsert_publication(&mut active.bundle, &repo, &pr);
        return Ok(StepOutcome {
            success: true,
            detail: "already merged".to_string(),
            publication_url: Some(publication.url),
            stdout: None,
            stderr: None,
            exit_code: None,
        });
    }
    ensure_open_and_ready(repo_id, &pr)?;

    let mut detail = Vec::new();
    if step.wait_for_checks.unwrap_or(true) {
        let summary = github::wait_for_checks(
            &cwd,
            &publication.url,
            step.required_checks_only.unwrap_or(true),
            step.timeout_seconds.unwrap_or(1800),
            step.interval_seconds.unwrap_or(10),
        )?;
        detail.push(format!("checks {}", summary.status));
    }

    let method = step.method.as_deref().unwrap_or("squash");
    github::merge_pr(
        &cwd,
        &publication.url,
        method,
        step.delete_branch.unwrap_or(false),
        pr.head_ref_oid.as_deref(),
    )?;
    let refreshed = github::view_pr(&cwd, &publication.url).unwrap_or_else(|_| PullRequest {
        state: Some("MERGED".to_string()),
        ..pr.clone()
    });
    github::upsert_publication(&mut active.bundle, &repo, &refreshed);
    detail.push(format!("merged with {method}"));

    if plan.provider != github::PROVIDER {
        bail!("unsupported land provider `{}`", plan.provider);
    }

    Ok(StepOutcome {
        success: true,
        detail: detail.join("; "),
        publication_url: Some(publication.url),
        stdout: None,
        stderr: None,
        exit_code: None,
    })
}

fn execute_wait_checks(active: &ActiveBundle, step: &LandStep) -> Result<StepOutcome> {
    let repo_id = required_repo_id(step)?;
    let (_, _, cwd) = repo_context(active, repo_id)?;
    let publication = publication_for_repo(&active.bundle, repo_id)
        .with_context(|| format!("{repo_id}: no GitHub PR publication recorded"))?;
    let summary = github::wait_for_checks(
        &cwd,
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
    })
}

fn validate_plan_for_bundle(active: &ActiveBundle, plan: &LandPlan) -> Result<()> {
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
                validate_merge_method(step.method.as_deref().unwrap_or("squash"))?;
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
            step_type => bail!("unknown land step type `{step_type}` in `{}`", step.id),
        }
    }
    Ok(())
}

fn preflight_publications(
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
        let (_, _, cwd) = repo_context(active, repo_id)?;
        let publication = publication_for_repo(&active.bundle, repo_id)
            .with_context(|| format!("{repo_id}: missing GitHub PR publication"))?;
        let pr = github::view_pr(&cwd, &publication.url)?;
        if state_is_merged(&pr) && run.is_some() {
            continue;
        }
        ensure_open_and_ready(repo_id, &pr)?;
        if step.step_type == STEP_WAIT_CHECKS || step.wait_for_checks.unwrap_or(true) {
            let runs = github::check_runs(
                &cwd,
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

fn ensure_checks_ready(repo_id: &str, runs: &[github::CheckRun]) -> Result<()> {
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

fn ordered_step_ids(steps: &[LandStep]) -> Result<Vec<String>> {
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

fn ensure_needs_succeeded(run: &LandRun, step: &LandStep) -> Result<()> {
    for need in &step.needs {
        let Some(run_step) = run_step(run, need) else {
            bail!("run is missing dependency step `{need}`");
        };
        if run_step.status != STATUS_SUCCEEDED {
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

fn print_plan(plan: &LandPlan, path: &Path) {
    println!("{} {}", out::heading("Land plan"), out::node(&plan.id));
    println!("{} {}", out::heading("Provider:"), plan.provider);
    println!(
        "{} {}",
        out::heading("Plan file:"),
        out::path(path.display())
    );
    if plan.provider == "github" {
        println!(
            "{} each recorded GitHub PR base branch",
            out::heading("Lands into:")
        );
    }
    println!();
    for step in &plan.steps {
        println!(
            "{} {} {}",
            out::node(&step.id),
            out::heading(&step.step_type),
            step.repo_id
                .as_deref()
                .map(out::repo)
                .unwrap_or_else(|| step.command.join(" "))
        );
        if !step.needs.is_empty() {
            println!("  needs {}", step.needs.join(", "));
        }
    }
    println!();
    println!("{} knit land apply", out::heading("Apply:"));
}

fn print_run_status(active: &ActiveBundle, run: &LandRun, path: &Path) {
    println!(
        "{} {} {}",
        out::heading("Land run"),
        out::node(&run.id),
        out::status(&run.status)
    );
    println!(
        "{} {}",
        out::heading("Run file:"),
        out::path(path.display())
    );
    if run.provider == "github" {
        println!(
            "{} each recorded GitHub PR base branch",
            out::heading("Lands into:")
        );
    }
    println!();
    for step in &run.steps {
        println!(
            "{} {} {}",
            out::node(&step.id),
            out::status(&format!("{:<9}", step.status)),
            step.detail.as_deref().unwrap_or("")
        );
        if let Some(repo_id) = &step.repo_id {
            print_pr_status(active, repo_id);
        }
        if let Some(stderr) = &step.stderr {
            if !stderr.trim().is_empty() {
                println!("  {}", out::danger(stderr.trim()));
            }
        }
    }
}

fn print_planned_step(active: &ActiveBundle, step: &LandStep) {
    println!(
        "{} {} {}",
        out::node(&step.id),
        out::muted("planned"),
        step.step_type
    );
    if let Some(repo_id) = &step.repo_id {
        print_pr_status(active, repo_id);
    }
}

fn print_pr_status(active: &ActiveBundle, repo_id: &str) {
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
    let Some(publication) = publication_for_repo(&active.bundle, repo_id) else {
        println!(
            "  {} {}",
            out::repo(repo_id),
            out::danger("publication missing")
        );
        return;
    };
    match github::view_pr(&cwd, &publication.url) {
        Ok(pr) => {
            println!(
                "  {} #{} {} {}",
                out::repo(repo_id),
                pr.number,
                out::status(pr.state.as_deref().unwrap_or("UNKNOWN")),
                pr.url
            );
            match github::check_runs(&cwd, &publication.url, true) {
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

fn check_status_label(runs: &[github::CheckRun]) -> String {
    if runs.is_empty() {
        return out::muted("no_required_checks");
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

fn new_run(active: &ActiveBundle, plan: &LandPlan, plan_path: &Path) -> LandRun {
    let now = now_iso();
    LandRun {
        schema_version: SCHEMA_VERSION.to_string(),
        kind: LAND_RUN_KIND.to_string(),
        id: format!("run-{}", safe_timestamp()),
        plan_id: plan.id.clone(),
        bundle_id: active.bundle.id.clone(),
        provider: plan.provider.clone(),
        plan_path: display_path_for_storage(&active.root, plan_path),
        status: STATUS_RUNNING.to_string(),
        created_at: now.clone(),
        updated_at: now,
        steps: plan
            .steps
            .iter()
            .map(|step| LandRunStep {
                id: step.id.clone(),
                step_type: step.step_type.clone(),
                status: STATUS_PENDING.to_string(),
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

fn repo_context(active: &ActiveBundle, repo_id: &str) -> Result<(usize, RepoEntry, PathBuf)> {
    let (index, repo) = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .find(|(_, repo)| repo.id == repo_id)
        .with_context(|| format!("{repo_id}: repo is not tracked in this bundle"))?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{repo_id}: no active checkout is recorded.");
    };
    ensure_expected_branch(repo, &cwd)?;
    Ok((index, repo.clone(), cwd))
}

fn step_publication(active: &ActiveBundle, step: &LandStep) -> Option<PublicationEntry> {
    let repo_id = step.repo_id.as_deref()?;
    publication_for_repo(&active.bundle, repo_id).cloned()
}

fn run_step<'a>(run: &'a LandRun, id: &str) -> Option<&'a LandRunStep> {
    run.steps.iter().find(|step| step.id == id)
}

fn required_repo_id(step: &LandStep) -> Result<&str> {
    step.repo_id
        .as_deref()
        .filter(|repo_id| !repo_id.trim().is_empty())
        .with_context(|| format!("step `{}` must provide repoId", step.id))
}

fn ensure_provider(provider: &str) -> Result<()> {
    if provider != github::PROVIDER {
        bail!("unsupported land provider `{provider}`. GitHub is the only provider implemented.");
    }
    Ok(())
}

fn validate_merge_method(method: &str) -> Result<()> {
    if !matches!(method, "squash" | "merge" | "rebase") {
        bail!("unknown merge method `{method}`; expected squash, merge, or rebase");
    }
    Ok(())
}

fn ensure_open_and_ready(repo_id: &str, pr: &PullRequest) -> Result<()> {
    if pr.is_draft.unwrap_or(false) {
        bail!("{repo_id}: PR #{} is a draft.", pr.number);
    }
    match pr.state.as_deref().unwrap_or("UNKNOWN") {
        "OPEN" => Ok(()),
        state => bail!("{repo_id}: PR #{} is {state}, expected OPEN.", pr.number),
    }
}

fn state_is_merged(pr: &PullRequest) -> bool {
    pr.state.as_deref() == Some("MERGED")
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

fn default_plan_path(active: &ActiveBundle) -> PathBuf {
    active
        .root
        .join(".knit/land-plans")
        .join(format!("{}.land.json", active.bundle.id))
}

fn new_run_path(active: &ActiveBundle, plan: &LandPlan) -> PathBuf {
    active
        .root
        .join(".knit/land-runs")
        .join(format!("{}-{}.run.json", plan.id, safe_timestamp()))
}

fn resolve_land_plan_path(active: &ActiveBundle, path: Option<&Path>) -> Result<PathBuf> {
    Ok(path
        .map(resolve_user_path)
        .unwrap_or_else(|| default_plan_path(active)))
}

fn resolve_land_run_path(active: &ActiveBundle, path: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = path {
        return Ok(Some(resolve_user_path(path)));
    }
    latest_run_path(active)
}

fn latest_run_path(active: &ActiveBundle) -> Result<Option<PathBuf>> {
    let dir = active.root.join(".knit/land-runs");
    if !dir.exists() {
        return Ok(None);
    }
    let mut paths = fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths.pop())
}

fn resolve_user_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn resolve_stored_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn display_path_for_storage(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn safe_timestamp() -> String {
    now_iso()
        .trim_end_matches('Z')
        .replace('T', "-")
        .replace(':', "")
        .replace('.', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, needs: &[&str]) -> LandStep {
        LandStep {
            id: id.to_string(),
            step_type: STEP_RUN.to_string(),
            needs: needs.iter().map(|need| need.to_string()).collect(),
            repo_id: None,
            method: None,
            wait_for_checks: None,
            required_checks_only: None,
            delete_branch: None,
            required_only: None,
            timeout_seconds: None,
            interval_seconds: None,
            cwd: Some(".".to_string()),
            command: vec!["true".to_string()],
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn orders_steps_by_dependencies() {
        let steps = vec![
            step("deploy", &["merge-a", "merge-b"]),
            step("merge-a", &[]),
            step("merge-b", &[]),
        ];
        let order = ordered_step_ids(&steps).unwrap();
        assert!(
            order.iter().position(|id| id == "merge-a")
                < order.iter().position(|id| id == "deploy")
        );
        assert!(
            order.iter().position(|id| id == "merge-b")
                < order.iter().position(|id| id == "deploy")
        );
    }

    #[test]
    fn rejects_dependency_cycles() {
        let steps = vec![step("a", &["b"]), step("b", &["a"])];
        assert!(ordered_step_ids(&steps)
            .unwrap_err()
            .to_string()
            .contains("cycle"));
    }

    #[test]
    fn rejects_unknown_merge_methods() {
        assert!(validate_merge_method("squash").is_ok());
        assert!(validate_merge_method("octopus")
            .unwrap_err()
            .to_string()
            .contains("unknown merge method"));
    }

    #[test]
    fn resume_dependencies_must_have_succeeded() {
        let run = LandRun {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: LAND_RUN_KIND.to_string(),
            id: "run".to_string(),
            plan_id: "plan".to_string(),
            bundle_id: "bundle".to_string(),
            provider: github::PROVIDER.to_string(),
            plan_path: "plan.json".to_string(),
            status: STATUS_RUNNING.to_string(),
            created_at: now_iso(),
            updated_at: now_iso(),
            steps: vec![LandRunStep {
                id: "a".to_string(),
                step_type: STEP_RUN.to_string(),
                status: STATUS_FAILED.to_string(),
                repo_id: None,
                publication_url: None,
                started_at: None,
                finished_at: None,
                detail: None,
                stdout: None,
                stderr: None,
                exit_code: None,
            }],
        };
        assert!(ensure_needs_succeeded(&run, &step("b", &["a"]))
            .unwrap_err()
            .to_string()
            .contains("waiting"));
    }
}
