use crate::advice;
use crate::checkout::{checkout_dir, ensure_expected_branch};
use crate::git::{current_branch, git_output, is_ancestor, is_git_worktree, rev_list, rev_parse};
use crate::ids::{node_id, slugify};
use crate::model::{
    BundleNode, KnitProject, ProjectLandingMergePlan, PublicationEntry, RepoChange, RepoEntry,
    DEFAULT_LANDING_MERGE_METHOD, SCHEMA_VERSION,
};
use crate::output as out;
use crate::providers::{self, publication_for_repo, CheckRun, Forge, PrTarget, PullRequest};
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, load_config, project_path, read_json,
    save_active_bundle, write_json, ActiveBundle,
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
const STEP_DEPLOY: &str = "deploy";
const STATUS_PENDING: &str = "pending";
const STATUS_RUNNING: &str = "running";
const STATUS_SUCCEEDED: &str = "succeeded";
const STATUS_FAILED: &str = "failed";
const DEFAULT_LAND_PROVIDER: &str = "github";
const DEPLOY_MODE_COMMAND: &str = "command";
const DEPLOY_MODE_PUSH: &str = "push";

pub fn apply_land_from_artifact(artifact_path: &Path, out_path: Option<&Path>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let mut bundle: crate::model::ChangeGroup = read_json(artifact_path)
        .with_context(|| format!("failed to load bundle artifact {}", artifact_path.display()))?;
    if bundle.repos.is_empty() {
        bail!("Bundle artifact has no repos.");
    }
    if bundle.publications.is_empty() {
        bail!("Bundle artifact has no review publications. Run publish first.");
    }

    let started_at = now_iso();
    let mut merged_repo_ids = Vec::new();
    let mut publication_urls = Vec::new();

    let repos = bundle.repos.clone();

    for repo in &repos {
        let Some(publication) = publication_for_repo(&bundle, &repo.id).cloned() else {
            continue;
        };
        let forge = providers::for_repo(repo)?;
        let target = artifact_target(&cwd, forge.as_ref(), repo)?;

        let pr = forge.view(&target, &publication.url)?;
        if state_is_merged(&pr) {
            providers::upsert_publication(&mut bundle, repo, forge.as_ref(), &pr);
            merged_repo_ids.push(repo.id.clone());
            publication_urls.push(publication.url.clone());
            println!(
                "{} {} {}",
                out::ok("already merged"),
                out::repo(&repo.id),
                out::muted(&publication.url)
            );
            continue;
        }

        ensure_open_and_ready(&repo.id, &pr)?;

        let summary = forge.wait_for_checks(&target, &publication.url, true, 1800, 10)?;
        println!(
            "{} {} {}",
            out::ok("checks"),
            out::repo(&repo.id),
            out::muted(&summary.status)
        );

        forge.merge(
            &target,
            &publication.url,
            DEFAULT_LANDING_MERGE_METHOD,
            false,
            pr.head_ref_oid.as_deref(),
        )?;

        let refreshed = forge.view(&target, &publication.url)?;
        providers::upsert_publication(&mut bundle, repo, forge.as_ref(), &refreshed);
        merged_repo_ids.push(repo.id.clone());
        publication_urls.push(publication.url.clone());
        println!(
            "{} {} {}",
            out::ok("merged"),
            out::repo(&repo.id),
            out::muted(&publication.url)
        );
    }

    // Record a landed node in the artifact without writing land plan/run files.
    let node = BundleNode::feature_landed(
        node_id("land"),
        started_at,
        format!("land-{}", bundle.id),
        format!("run-artifact-{}", bundle.id),
        DEFAULT_LAND_PROVIDER.to_string(),
        merged_repo_ids,
        publication_urls,
    );
    bundle.nodes.push(node);
    bundle.head_node_id = bundle.nodes.last().map(|node| node.id.clone());
    bundle.updated_at = now_iso();

    match out_path {
        Some(path) => write_json(path, &bundle),
        None => {
            let json = serde_json::to_string_pretty(&bundle).context("failed to encode bundle JSON")?;
            println!("{json}");
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LandPlan {
    schema_version: String,
    kind: String,
    id: String,
    provider: String,
    bundle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_project_id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deployment_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkout: Option<LandCheckout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LandCheckout {
    branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    update: Option<String>,
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
    publication_update: Option<PublicationUpdate>,
}

#[derive(Debug)]
struct PublicationUpdate {
    repo: RepoEntry,
    pr: PullRequest,
}

pub fn generate_land_plan(
    provider: Option<&str>,
    out_path: Option<&Path>,
    force: bool,
) -> Result<()> {
    let active = load_active_bundle()?;
    let plan = build_default_plan(&active, provider)?;
    validate_plan_for_bundle(&active, &plan)?;
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
    generate_land_plan(None, None, false)
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
    if providers::by_id(&plan.provider).is_some() {
        println!(
            "{} each recorded review object's base branch",
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
        // Mirror the pushed feature branches into the KnitHub remote bundle
        // (default on; see `knit config set push-sync`).
        crate::commands::remote::maybe_sync_bundle_to_remote(None, false)?;
    }

    Ok(())
}

fn build_default_plan(active: &ActiveBundle, requested_provider: Option<&str>) -> Result<LandPlan> {
    let project = load_project_for_bundle(active)?;
    let landing = project
        .as_ref()
        .and_then(|project| project.landing.as_ref());
    let provider = requested_provider
        .or_else(|| landing.and_then(|landing| landing.provider.as_deref()))
        .unwrap_or(DEFAULT_LAND_PROVIDER)
        .to_string();
    ensure_provider(&provider)?;
    let merge = landing.map(|landing| &landing.merge);
    let mut steps = Vec::new();
    let ordered_ids: BTreeSet<String> = merge
        .map(|m| m.repo_order.iter().cloned().collect())
        .unwrap_or_default();
    let empty_needs = BTreeMap::new();
    let merge_needs = merge.map(|m| &m.needs).unwrap_or(&empty_needs);
    let mut previous_ordered: Option<String> = None;
    for repo in ordered_merge_repos(active, merge) {
        if publication_for_repo(&active.bundle, &repo.id).is_none() {
            continue;
        }
        let id = format!("merge-{}", repo.id);
        let needs = if let Some(explicit_needs) = merge_needs.get(&repo.id) {
            explicit_needs.clone()
        } else if ordered_ids.contains(&repo.id) {
            previous_ordered.iter().cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        steps.push(LandStep {
            id: id.clone(),
            step_type: STEP_MERGE_PR.to_string(),
            needs,
            repo_id: Some(repo.id.clone()),
            method: Some(merge_method(merge)),
            wait_for_checks: Some(merge_wait_for_checks(merge)),
            required_checks_only: Some(merge_required_checks_only(merge)),
            delete_branch: Some(merge_delete_branch(merge)),
            required_only: None,
            timeout_seconds: Some(merge_timeout_seconds(merge)),
            interval_seconds: Some(merge_interval_seconds(merge)),
            cwd: None,
            command: Vec::new(),
            env: BTreeMap::new(),
            deployment_mode: None,
            checkout: None,
        });
        if ordered_ids.contains(&repo.id) {
            previous_ordered = Some(id);
        }
    }
    append_project_deployments(active, landing, &mut steps)?;

    if steps.is_empty() {
        bail!(
            "No GitHub PR publications or project landing deployments are available for this bundle. Run `knit publish github create` first or configure project landing deployments."
        );
    }

    Ok(LandPlan {
        schema_version: SCHEMA_VERSION.to_string(),
        kind: LAND_PLAN_KIND.to_string(),
        id: format!("land-{}", active.bundle.id),
        provider,
        bundle_id: active.bundle.id.clone(),
        source_project_id: project.map(|project| project.id),
        created_at: now_iso(),
        steps,
    })
}

fn load_project_for_bundle(active: &ActiveBundle) -> Result<Option<KnitProject>> {
    let config = load_config(&active.root)?;
    let Some(project_id) = active
        .bundle
        .project_id
        .as_deref()
        .or(config.active_project.as_deref())
    else {
        return Ok(None);
    };
    read_json(&project_path(&active.root, project_id)).map(Some)
}

fn ordered_merge_repos<'a>(
    active: &'a ActiveBundle,
    merge: Option<&ProjectLandingMergePlan>,
) -> Vec<&'a RepoEntry> {
    let mut repos = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(merge) = merge {
        for repo_id in &merge.repo_order {
            if let Some(repo) = active.bundle.repos.iter().find(|repo| repo.id == *repo_id) {
                if seen.insert(repo.id.clone()) {
                    repos.push(repo);
                }
            }
        }
    }

    if merge
        .and_then(|merge| merge.include_unlisted)
        .unwrap_or(true)
    {
        for repo in &active.bundle.repos {
            if seen.insert(repo.id.clone()) {
                repos.push(repo);
            }
        }
    }

    repos
}

fn merge_method(merge: Option<&ProjectLandingMergePlan>) -> String {
    merge
        .and_then(|merge| merge.method.clone())
        .unwrap_or_else(|| DEFAULT_LANDING_MERGE_METHOD.to_string())
}

fn merge_wait_for_checks(merge: Option<&ProjectLandingMergePlan>) -> bool {
    merge
        .and_then(|merge| merge.wait_for_checks)
        .unwrap_or(true)
}

fn merge_required_checks_only(merge: Option<&ProjectLandingMergePlan>) -> bool {
    merge
        .and_then(|merge| merge.required_checks_only)
        .unwrap_or(true)
}

fn merge_delete_branch(merge: Option<&ProjectLandingMergePlan>) -> bool {
    merge.and_then(|merge| merge.delete_branch).unwrap_or(false)
}

fn merge_timeout_seconds(merge: Option<&ProjectLandingMergePlan>) -> u64 {
    merge
        .and_then(|merge| merge.timeout_seconds)
        .unwrap_or(1800)
}

fn merge_interval_seconds(merge: Option<&ProjectLandingMergePlan>) -> u64 {
    merge.and_then(|merge| merge.interval_seconds).unwrap_or(10)
}

fn append_project_deployments(
    active: &ActiveBundle,
    landing: Option<&crate::model::ProjectLandingPlan>,
    steps: &mut Vec<LandStep>,
) -> Result<()> {
    let Some(landing) = landing else {
        return Ok(());
    };
    let merge_step_ids = steps
        .iter()
        .filter(|step| step.step_type == STEP_MERGE_PR)
        .filter_map(|step| Some((step.repo_id.clone()?, step.id.clone())))
        .collect::<BTreeMap<_, _>>();
    let all_merge_ids = steps
        .iter()
        .filter(|step| step.step_type == STEP_MERGE_PR)
        .map(|step| step.id.clone())
        .collect::<Vec<_>>();

    for deployment in &landing.deployments {
        if let Some(repo_id) = &deployment.repo_id {
            if !active.bundle.repos.iter().any(|repo| repo.id == *repo_id) {
                continue;
            }
        }
        let mode = deployment.mode.clone().unwrap_or_else(|| {
            if deployment.command.is_empty() {
                DEPLOY_MODE_PUSH.to_string()
            } else {
                DEPLOY_MODE_COMMAND.to_string()
            }
        });
        let needs = if deployment.needs.is_empty() {
            default_deployment_needs(
                deployment.repo_id.as_deref(),
                &merge_step_ids,
                &all_merge_ids,
            )
        } else {
            deployment.needs.clone()
        };
        let checkout = deployment.checkout.as_ref().map(|checkout| LandCheckout {
            branch: checkout.branch.clone(),
            remote: checkout.remote.clone(),
            update: checkout.update.clone(),
        });
        steps.push(LandStep {
            id: deployment.id.clone(),
            step_type: STEP_DEPLOY.to_string(),
            needs,
            repo_id: deployment.repo_id.clone(),
            method: None,
            wait_for_checks: None,
            required_checks_only: None,
            delete_branch: None,
            required_only: None,
            timeout_seconds: None,
            interval_seconds: None,
            cwd: deployment.cwd.clone(),
            command: deployment.command.clone(),
            env: deployment.env.clone(),
            deployment_mode: Some(mode),
            checkout,
        });
    }

    Ok(())
}

fn default_deployment_needs(
    repo_id: Option<&str>,
    merge_step_ids: &BTreeMap<String, String>,
    all_merge_ids: &[String],
) -> Vec<String> {
    if let Some(repo_id) = repo_id {
        if let Some(merge_step) = merge_step_ids.get(repo_id) {
            return vec![merge_step.clone()];
        }
    }
    all_merge_ids.to_vec()
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
    let provider = bundle_primary_provider(active);
    active.bundle.nodes.push(BundleNode::land_update(
        node_id("land_update"),
        now.clone(),
        provider,
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
        let repo = active.bundle.repos[target.repo_index].clone();
        let forge = providers::for_repo(&repo)?;
        let pr_target = PrTarget::checkout(&target.cwd);
        let pr = forge
            .view(&pr_target, &target.publication_url)
            .with_context(|| format!("{}: failed to refresh PR metadata", target.repo_id))?;
        providers::upsert_publication(&mut active.bundle, &repo, forge.as_ref(), &pr);
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
            pending.push((step, run_index));
        }

        if pending.is_empty() {
            continue;
        }

        run.status = STATUS_RUNNING.to_string();
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
                STATUS_SUCCEEDED.to_string()
            } else {
                STATUS_FAILED.to_string()
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
            STATUS_FAILED.to_string()
        } else {
            STATUS_RUNNING.to_string()
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

    run.status = STATUS_SUCCEEDED.to_string();
    run.updated_at = now_iso();
    write_json(run_path, run)?;
    append_landed_node(active, plan, run)?;
    save_active_bundle(active)?;
    println!("{} {}", out::heading("Feature landed"), out::node(&run.id));
    Ok(())
}

fn step_waves(steps: &[LandStep], order: &[String]) -> Result<Vec<Vec<String>>> {
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

fn execute_step(
    active: &ActiveBundle,
    plan: &LandPlan,
    step: &LandStep,
) -> Result<StepOutcome> {
    match step.step_type.as_str() {
        STEP_MERGE_PR => execute_merge_pr(active, plan, step),
        STEP_WAIT_CHECKS => execute_wait_checks(active, step),
        STEP_RUN => execute_run_command(active, step),
        STEP_DEPLOY => execute_deployment(active, step),
        step_type => bail!("unknown land step type `{step_type}`"),
    }
}

fn execute_merge_pr(
    active: &ActiveBundle,
    plan: &LandPlan,
    step: &LandStep,
) -> Result<StepOutcome> {
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

    let method = step
        .method
        .as_deref()
        .unwrap_or(DEFAULT_LANDING_MERGE_METHOD);
    forge.merge(
        &target,
        &publication.url,
        method,
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
    let mode = step
        .deployment_mode
        .as_deref()
        .unwrap_or(if step.command.is_empty() {
            DEPLOY_MODE_PUSH
        } else {
            DEPLOY_MODE_COMMAND
        });
    validate_deployment_mode(mode)?;
    let repo_id = required_repo_id(step)?;

    if mode == DEPLOY_MODE_PUSH {
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

fn deployment_cwd(
    active: &ActiveBundle,
    repo_id: &str,
    step: &LandStep,
) -> Result<(PathBuf, String)> {
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
    let update = checkout.update.as_deref().unwrap_or("fetch");
    validate_deploy_checkout_update(update)?;
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

    let target_ref = if update == "none" {
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
        let (_, repo, cwd) = repo_context(active, repo_id)?;
        let forge = providers::for_repo(&repo)?;
        let target = PrTarget::checkout(&cwd);
        let publication = publication_for_repo(&active.bundle, repo_id)
            .with_context(|| format!("{repo_id}: missing review publication"))?;
        let pr = forge.view(&target, &publication.url)?;
        if state_is_merged(&pr) && run.is_some() {
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

fn print_plan(plan: &LandPlan, path: &Path) {
    println!("{} {}", out::heading("Land plan"), out::node(&plan.id));
    println!("{} {}", out::heading("Provider:"), plan.provider);
    println!(
        "{} {}",
        out::heading("Plan file:"),
        out::path(path.display())
    );
    if providers::by_id(&plan.provider).is_some() {
        println!(
            "{} each recorded review object's base branch",
            out::heading("Lands into:")
        );
    }
    println!();
    for step in &plan.steps {
        println!(
            "{} {} {}",
            out::node(&step.id),
            out::heading(&step.step_type),
            planned_step_target(step)
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
    if providers::by_id(&run.provider).is_some() {
        println!(
            "{} each recorded review object's base branch",
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
            print_pr_status(active, repo_id, step.publication_url.as_deref());
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
        print_pr_status(active, repo_id, None);
    }
}

fn planned_step_target(step: &LandStep) -> String {
    match step.step_type.as_str() {
        STEP_DEPLOY => {
            let repo = step
                .repo_id
                .as_deref()
                .map(out::repo)
                .unwrap_or_else(|| out::muted("workspace"));
            let mode = step.deployment_mode.as_deref().unwrap_or_else(|| {
                if step.command.is_empty() {
                    DEPLOY_MODE_PUSH
                } else {
                    DEPLOY_MODE_COMMAND
                }
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
    if providers::by_id(provider).is_none() {
        bail!("unsupported land provider `{provider}`. Supported: github, gitlab, forgejo.");
    }
    Ok(())
}

/// Build a forge target for artifact landing, scoping to the repo's full name
/// when the remote is recognized so the CLI can target it without a checkout.
fn artifact_target(cwd: &Path, forge: &dyn Forge, repo: &RepoEntry) -> Result<PrTarget> {
    match repo
        .remote
        .as_deref()
        .and_then(|remote| forge.repo_full_name(remote))
    {
        Some(full_name) => Ok(PrTarget::explicit(cwd, full_name)),
        None => Ok(PrTarget::checkout(cwd)),
    }
}

/// Best-effort provider label for a bundle, used on informational ledger nodes.
fn bundle_primary_provider(active: &ActiveBundle) -> String {
    active
        .bundle
        .repos
        .iter()
        .find_map(|repo| providers::for_repo(repo).ok().map(|forge| forge.id().to_string()))
        .unwrap_or_else(|| DEFAULT_LAND_PROVIDER.to_string())
}

fn validate_merge_method(method: &str) -> Result<()> {
    if !matches!(method, "squash" | "merge" | "rebase") {
        bail!("unknown merge method `{method}`; expected squash, merge, or rebase");
    }
    Ok(())
}

fn validate_deployment_mode(mode: &str) -> Result<()> {
    if !matches!(mode, DEPLOY_MODE_COMMAND | DEPLOY_MODE_PUSH) {
        bail!("unknown deployment mode `{mode}`; expected command or push");
    }
    Ok(())
}

fn validate_deploy_checkout_update(update: &str) -> Result<()> {
    if !matches!(update, "fetch" | "pull" | "none") {
        bail!("unknown deploy checkout update `{update}`; expected fetch, pull, or none");
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
    for path in paths.into_iter().rev() {
        let run: LandRun = read_json(&path)?;
        if run.bundle_id == active.bundle.id {
            return Ok(Some(path));
        }
    }
    Ok(None)
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

fn resolve_subdir(base: &Path, subdir: &str) -> PathBuf {
    let path = PathBuf::from(subdir);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
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
            deployment_mode: None,
            checkout: None,
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
            provider: "github".to_string(),
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

    #[test]
    fn latest_run_path_ignores_other_bundles() {
        let root = std::env::temp_dir().join(format!(
            "knit-land-run-test-{}-{}",
            std::process::id(),
            safe_timestamp()
        ));
        let run_dir = root.join(".knit/land-runs");
        std::fs::create_dir_all(&run_dir).unwrap();
        let active = ActiveBundle::unlocked(
            root.clone(),
            root.join(".knit/bundles/target.bundle.json"),
            crate::model::ChangeGroup::new("target".to_string(), "target".to_string(), now_iso()),
        );
        write_test_run(&run_dir.join("001.run.json"), "other");
        write_test_run(&run_dir.join("002.run.json"), "target");
        write_test_run(&run_dir.join("003.run.json"), "other");

        let path = latest_run_path(&active).unwrap().unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("002.run.json")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn step_waves_groups_independent_steps() {
        let steps = vec![
            step("merge-a", &[]),
            step("merge-b", &[]),
            step("deploy", &["merge-a", "merge-b"]),
        ];
        let order = ordered_step_ids(&steps).unwrap();
        let waves = step_waves(&steps, &order).unwrap();
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0].len(), 2);
        assert!(waves[0].contains(&"merge-a".to_string()));
        assert!(waves[0].contains(&"merge-b".to_string()));
        assert_eq!(waves[1], vec!["deploy".to_string()]);
    }

    #[test]
    fn step_waves_respects_sequential_chain() {
        let steps = vec![
            step("merge-a", &[]),
            step("merge-b", &["merge-a"]),
            step("merge-c", &["merge-b"]),
        ];
        let order = ordered_step_ids(&steps).unwrap();
        let waves = step_waves(&steps, &order).unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0], vec!["merge-a".to_string()]);
        assert_eq!(waves[1], vec!["merge-b".to_string()]);
        assert_eq!(waves[2], vec!["merge-c".to_string()]);
    }

    #[test]
    fn step_waves_mixed_parallel_and_sequential() {
        let steps = vec![
            step("merge-a", &[]),
            step("merge-b", &["merge-a"]),
            step("merge-c", &[]),
            step("deploy", &["merge-b", "merge-c"]),
        ];
        let order = ordered_step_ids(&steps).unwrap();
        let waves = step_waves(&steps, &order).unwrap();
        assert_eq!(waves.len(), 3);
        assert!(waves[0].contains(&"merge-a".to_string()));
        assert!(waves[0].contains(&"merge-c".to_string()));
        assert_eq!(waves[1], vec!["merge-b".to_string()]);
        assert_eq!(waves[2], vec!["deploy".to_string()]);
    }

    #[test]
    fn default_deployment_needs_uses_repo_merge_step() {
        let mut merge_ids = BTreeMap::new();
        merge_ids.insert("backend".to_string(), "merge-backend".to_string());
        let all_merge = vec!["merge-backend".to_string(), "merge-frontend".to_string()];
        let needs = default_deployment_needs(Some("backend"), &merge_ids, &all_merge);
        assert_eq!(needs, vec!["merge-backend".to_string()]);
    }

    #[test]
    fn default_deployment_needs_falls_back_to_all_merges() {
        let merge_ids = BTreeMap::new();
        let all_merge = vec!["merge-a".to_string(), "merge-b".to_string()];
        let needs = default_deployment_needs(None, &merge_ids, &all_merge);
        assert_eq!(needs, vec!["merge-a".to_string(), "merge-b".to_string()]);
    }

    #[test]
    fn step_waves_merge_depends_on_deploy() {
        let steps = vec![
            step("merge-a", &[]),
            step("deploy-a", &["merge-a"]),
            step("merge-b", &["deploy-a"]),
        ];
        let order = ordered_step_ids(&steps).unwrap();
        let waves = step_waves(&steps, &order).unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0], vec!["merge-a".to_string()]);
        assert_eq!(waves[1], vec!["deploy-a".to_string()]);
        assert_eq!(waves[2], vec!["merge-b".to_string()]);
    }

    fn write_test_run(path: &Path, bundle_id: &str) {
        let run = LandRun {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: LAND_RUN_KIND.to_string(),
            id: format!("run-{bundle_id}"),
            plan_id: "plan".to_string(),
            bundle_id: bundle_id.to_string(),
            provider: "github".to_string(),
            plan_path: "plan.json".to_string(),
            status: STATUS_SUCCEEDED.to_string(),
            created_at: now_iso(),
            updated_at: now_iso(),
            steps: Vec::new(),
        };
        let text = serde_json::to_string_pretty(&run).unwrap();
        std::fs::write(path, format!("{text}\n")).unwrap();
    }
}
