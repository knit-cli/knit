//! `knit land` — create and execute a landing plan for the resolved bundle.
//!
//! This module is the public command surface and the home of the shared land
//! plan/run data types, constants, and small cross-cutting helpers. The heavy
//! lifting lives in focused submodules:
//!
//! - [`plan`] builds the default plan from the bundle and project landing template
//! - [`check`] live landing-readiness preflight (`knit land check`)
//! - [`validate`] validates a plan and preflights its PRs
//! - [`execute`] schedules and runs the plan, recording progress
//! - [`update`] implements `knit land update` (merge base into feature branches)
//! - [`display`] renders plans, run status, and PR/check state

mod check;
mod display;
mod execute;
mod plan;
mod update;
mod validate;

pub use check::check_landing;
pub(crate) use check::{assess_landing_readiness, print_readiness_row};

use crate::advice;
use crate::checkout::{checkout_dir, ensure_expected_branch};
use crate::ids::node_id;
use crate::model::{BundleNode, RepoEntry, DEFAULT_LANDING_MERGE_METHOD};
use crate::output as out;
use crate::providers::{self, publication_for_repo, Forge, PrTarget, PullRequest};
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, read_json, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

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
        )
        .with_context(|| format!("{}: merging {}", repo.id, publication.url))?;

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
    let plan = plan::build_default_plan(&active, provider)?;
    validate::validate_plan_for_bundle(&active, &plan)?;
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
    display::print_plan(&plan, &path);
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
        display::print_run_status(&active, &run, &path);
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
        validate::validate_plan_for_bundle(&active, &plan)?;
        display::print_plan(&plan, &plan_path);
        advice::print(
            &active.root,
            "inspect or edit this plan, then run `knit land apply` when you are ready to execute it.",
        );
        return Ok(());
    }

    drop(active);
    generate_land_plan(None, None, false)
}

pub fn apply_land_plan(plan_path: Option<&Path>, remote: &[String], no_remote: bool) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let path = resolve_land_plan_path(&active, plan_path)?;
    if !path.exists() {
        bail!(
            "No land plan found at {}. Run `knit land plan` first, inspect the plan, then run `knit land apply`.",
            path.display()
        );
    }
    let plan: LandPlan = read_json(&path)?;
    validate::validate_plan_for_bundle(&active, &plan)?;
    let order = validate::ordered_step_ids(&plan.steps)?;
    validate::preflight_publications(&active, &plan, None)?;

    let run_path = new_run_path(&active, &plan);
    if let Some(parent) = run_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut run = execute::new_run(&active, &plan, &path);
    write_json(&run_path, &run)?;
    execute::execute_run(&mut active, &plan, &order, &mut run, &run_path)?;
    crate::commands::remote::sync_bundle_to_remote_if_enabled(remote, no_remote)?;
    Ok(())
}

pub fn resume_land_run(run_path: Option<&Path>, remote: &[String], no_remote: bool) -> Result<()> {
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
    validate::validate_plan_for_bundle(&active, &plan)?;
    let order = validate::ordered_step_ids(&plan.steps)?;
    validate::preflight_publications(&active, &plan, Some(&run))?;
    run.status = STATUS_RUNNING.to_string();
    run.updated_at = now_iso();
    write_json(&path, &run)?;
    execute::execute_run(&mut active, &plan, &order, &mut run, &path)?;
    crate::commands::remote::sync_bundle_to_remote_if_enabled(remote, no_remote)?;
    Ok(())
}

pub fn sync_landed_bundle(remote: &[String]) -> Result<()> {
    let active = load_active_bundle()?;
    if crate::commands::bundle::bundle_state(&active.bundle) != "landed" {
        bail!(
            "Bundle {} is not landed yet. Run `knit land apply` first.",
            active.bundle.id
        );
    }
    drop(active);
    crate::commands::remote::sync_bundle_to_remote(remote)
}

pub fn show_land_status(run_path: Option<&Path>) -> Result<()> {
    let active = load_active_bundle()?;
    if let Some(path) = resolve_land_run_path(&active, run_path)? {
        let run: LandRun = read_json(&path)?;
        display::print_run_status(&active, &run, &path);
        return Ok(());
    }

    let plan_path = default_plan_path(&active);
    if !plan_path.exists() {
        bail!("No land run or default land plan found. Run `knit land plan` first.");
    }
    let plan: LandPlan = read_json(&plan_path)?;
    validate::validate_plan_for_bundle(&active, &plan)?;
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
        display::print_planned_step(&active, step);
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
    update::run_branch_update(selectors, all, push, set_upstream, continue_merge)
}

// ---------------------------------------------------------------------------
// Shared helpers used across the land submodules.
// ---------------------------------------------------------------------------

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
        "OPEN" => {
            if pr.is_conflicting() {
                bail!(
                    "{repo_id}: PR #{} has merge conflicts with its base branch. Run `knit land update` to merge the base in and resolve them, then land again.",
                    pr.number
                );
            }
            Ok(())
        }
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
        .replace([':', '.'], "")
}

#[cfg(test)]
mod tests {
    use super::execute::{ensure_needs_succeeded, step_waves};
    use super::plan::default_deployment_needs;
    use super::validate::ordered_step_ids;
    use super::*;
    use crate::model::SCHEMA_VERSION;

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
