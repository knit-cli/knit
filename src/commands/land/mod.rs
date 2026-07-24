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
//! - [`rollback`] creates revert PRs for a failed run's merged steps
//! - [`update`] implements `knit land update` (merge base into feature branches)
//! - [`display`] renders plans, run status, and PR/check state

mod artifact;
mod check;
mod display;
mod execute;
mod plan;
mod process;
mod rollback;
mod types;
mod update;
mod validate;

pub use artifact::apply_land_from_artifact;
pub use check::check_landing;
pub(crate) use check::{assess_landing_readiness, print_readiness_row};
pub(crate) use process::DEFAULT_COMMAND_TIMEOUT_SECONDS;
pub use rollback::rollback_land_run;

use crate::advice;
use crate::checkout::{checkout_dir, ensure_expected_branch};
use crate::model::RepoEntry;
use crate::output as out;
use crate::providers::{self, publication_for_repo, Forge, PrTarget, PullRequest};
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, read_json, save_active_bundle, write_json,
    ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use types::*;

pub fn generate_land_plan(
    provider: Option<&str>,
    out_path: Option<&Path>,
    force: bool,
    target_branch: Option<&str>,
) -> Result<()> {
    let active = load_active_bundle()?;
    let target_branch = normalize_target_branch(target_branch)?;
    let plan = plan::build_default_plan(&active, provider, target_branch.as_deref())?;
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
    display::print_plan(&active, &plan, &path);
    let apply = target_branch
        .as_deref()
        .map(|target| format!("`knit land --target {target} apply`"))
        .unwrap_or_else(|| "`knit land apply`".to_string());
    advice::print(
        &active.root,
        format!("inspect or edit this plan, then run {apply} when you are ready to execute it."),
    );
    Ok(())
}

pub fn land_default(target_branch: Option<&str>) -> Result<()> {
    let active = load_active_bundle()?;
    let target_branch = normalize_target_branch(target_branch)?;
    if let Some(path) = resolve_land_run_path(&active, None)? {
        let run: LandRun = read_json(&path)?;
        if target_branch.is_some() {
            let plan_path = resolve_stored_path(&active.root, &run.plan_path);
            let plan: LandPlan = read_json(&plan_path)?;
            ensure_requested_target_matches_plan(target_branch.as_deref(), &plan)?;
        }
        display::print_run_status(&active, &run, &path);
        if run.status == LandStatus::Succeeded {
            return Ok(());
        }
        if run.rolled_back_at.is_some() {
            advice::print(
                &active.root,
                "this run was rolled back; its merged PRs have open revert PRs. Land those reverts or start over with `knit land apply` once the bundle is ready again.",
            );
            return Ok(());
        }
        advice::print(
            &active.root,
            "fix the failed or incomplete step, then run `knit land resume` when you are ready to continue execution. Use `knit land rollback` to revert the steps that already merged instead.",
        );
        return Ok(());
    }

    let plan_path = default_plan_path(&active);
    if plan_path.exists() {
        let plan: LandPlan = read_json(&plan_path)?;
        ensure_requested_target_matches_plan(target_branch.as_deref(), &plan)?;
        validate::validate_plan_for_bundle(&active, &plan)?;
        display::print_plan(&active, &plan, &plan_path);
        advice::print(
            &active.root,
            "inspect or edit this plan, then run `knit land apply` when you are ready to execute it.",
        );
        return Ok(());
    }

    drop(active);
    generate_land_plan(None, None, false, target_branch.as_deref())
}

pub fn apply_land_plan(
    plan_path: Option<&Path>,
    remote: &[String],
    no_remote: bool,
    skip_checks: bool,
    keep_worktrees: bool,
    tag: Option<String>,
    no_tag: bool,
    target_branch: Option<&str>,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let target_branch = normalize_target_branch(target_branch)?;
    let path = resolve_land_plan_path(&active, plan_path)?;
    if !path.exists() {
        bail!(
            "No land plan found at {}. Run `knit land plan` first, inspect the plan, then run `knit land apply`.",
            path.display()
        );
    }
    let plan: LandPlan = read_json(&path)?;
    ensure_requested_target_matches_plan(target_branch.as_deref(), &plan)?;
    validate::validate_plan_for_bundle(&active, &plan)?;
    validate::preflight_required_checks(&active, &plan.require_checks, skip_checks)?;
    let order = validate::ordered_step_ids(&plan.steps)?;
    prepare_plan_publication_targets(&mut active, &plan)?;
    validate::preflight_publications(&active, &plan, None)?;

    let run_path = new_run_path(&active, &plan);
    if let Some(parent) = run_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut run = execute::new_run(&active, &plan, &path);
    write_json(&run_path, &run)?;
    execute::execute_run(&mut active, &plan, &order, &mut run, &run_path)?;
    let removed_worktrees = archive_landed_bundle(&mut active, keep_worktrees)?;
    crate::commands::remote::sync_active_bundle_to_remote_if_enabled(&active, remote, no_remote)?;
    print_landed_summary(&active.bundle.id, removed_worktrees, keep_worktrees);
    tag_landed_bundle(&mut active, tag, no_tag, remote, no_remote);
    Ok(())
}

fn normalize_target_branch(target_branch: Option<&str>) -> Result<Option<String>> {
    let Some(target_branch) = target_branch else {
        return Ok(None);
    };
    let target_branch = target_branch.trim();
    if target_branch.is_empty() {
        bail!("--target must name a non-empty branch");
    }
    Ok(Some(target_branch.to_string()))
}

fn ensure_requested_target_matches_plan(
    requested_target: Option<&str>,
    plan: &LandPlan,
) -> Result<()> {
    let Some(requested_target) = requested_target else {
        return Ok(());
    };
    if plan.target_branch.as_deref() == Some(requested_target) {
        return Ok(());
    }
    let planned = plan.target_branch.as_deref().unwrap_or("recorded PR bases");
    bail!(
        "Land plan targets {planned}, not `{requested_target}`. Regenerate it with `knit land --target {requested_target} plan --force`, inspect it, then apply again."
    )
}

/// Apply the plan's native target contract to the recorded review objects
/// before target-specific readiness checks and merge execution. Each
/// successful retarget is recorded immediately so a partially interrupted
/// provider operation remains resumable and the bundle never lies about the
/// remote PR base.
fn prepare_plan_publication_targets(active: &mut ActiveBundle, plan: &LandPlan) -> Result<()> {
    let Some(target_branch) = plan.target_branch.as_deref() else {
        return Ok(());
    };
    let mut seen = std::collections::BTreeSet::new();
    for step in &plan.steps {
        if step.step_type != LandStepKind::MergePr {
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
            .with_context(|| format!("{repo_id}: missing review publication"))?
            .clone();
        let current = forge.view(&target, &publication.url)?;
        let current_base = current
            .base_ref_name
            .as_deref()
            .unwrap_or(&publication.base_branch);
        if current_base == target_branch {
            continue;
        }
        if state_is_merged(&current) {
            bail!(
                "{repo_id}: PR #{} already merged into `{current_base}` and cannot be landed into `{target_branch}`.",
                current.number
            );
        }
        ensure_open_for_retarget(repo_id, &current)?;
        forge
            .edit_base(&target, &publication.url, target_branch)
            .with_context(|| {
                format!(
                    "{repo_id}: failed to retarget PR #{} from `{current_base}` to `{target_branch}`",
                    current.number
                )
            })?;
        let refreshed = forge.view(&target, &publication.url)?;
        if refreshed.base_ref_name.as_deref() != Some(target_branch) {
            bail!(
                "{repo_id}: provider did not retarget PR #{} to `{target_branch}`",
                current.number
            );
        }
        providers::upsert_publication(&mut active.bundle, &repo, forge.as_ref(), &refreshed);
        save_active_bundle(active)?;
        println!(
            "{} {} PR #{} {} -> {}",
            out::ok("retargeted"),
            out::repo(repo_id),
            current.number,
            out::branch(current_base),
            out::branch(target_branch)
        );
    }
    Ok(())
}

/// Decide whether a just-landed bundle gets a known-good tag. An explicit
/// `--tag [name]` always tags (empty name defaults to the bundle slug); the
/// `auto-tag` config default tags unless `--no-tag` overrides. Tagging is
/// best-effort: the land already merged and archived, so a tag failure warns
/// and points at a retry rather than failing the whole command. When nothing
/// is tagged, the usual "you could tag" advice is printed instead.
fn tag_landed_bundle(
    active: &mut ActiveBundle,
    tag: Option<String>,
    no_tag: bool,
    remote: &[String],
    no_remote: bool,
) {
    let alternate_target = active.bundle.repos.iter().any(|repo| {
        publication_for_repo(&active.bundle, &repo.id)
            .is_some_and(|publication| publication.base_branch != repo.base_branch)
    });
    let auto_tag = !no_tag
        && crate::store::load_effective_config(&active.root)
            .map(|config| config.auto_tag_enabled())
            .unwrap_or(false);
    if alternate_target && tag.is_none() {
        if auto_tag {
            println!(
                "{} skipped automatic tag because this landing targeted an alternate review branch; `knit tag` pins configured project bases.",
                out::warn("warning:")
            );
        }
        return;
    }
    if alternate_target && tag.is_some() {
        println!(
            "{} explicit --tag records the configured project bases, not the alternate review target.",
            out::warn("warning:")
        );
    }
    let name = match tag {
        Some(name) if !name.is_empty() => name,
        Some(_) => active.bundle.id.clone(),
        None if auto_tag => active.bundle.id.clone(),
        None => {
            advice::print(
                &active.root,
                format!(
                    "after verifying the configured project bases, mark them known-good with `knit tag <name> --bundle {}`.",
                    active.bundle.id
                ),
            );
            return;
        }
    };

    if let Err(error) = crate::commands::tag::create_tag_on_active(
        active,
        &name,
        &[],
        None,
        false,
        false,
        remote,
        no_remote,
    ) {
        println!(
            "{} land succeeded but tagging failed: {error:#}",
            out::warn("warning:")
        );
        advice::print(
            &active.root,
            format!(
                "retry the tag with `knit tag {name} --bundle {}`.",
                active.bundle.id
            ),
        );
    }
}

pub fn resume_land_run(
    run_path: Option<&Path>,
    remote: &[String],
    no_remote: bool,
    skip_checks: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let path = resolve_land_run_path(&active, run_path)?
        .with_context(|| "No land run found. Run `knit land apply` first.")?;
    let mut run: LandRun = read_json(&path)?;
    if run.status == LandStatus::Succeeded {
        println!(
            "{} {} is already succeeded.",
            out::heading("Land run"),
            out::node(&run.id)
        );
        return Ok(());
    }
    if let Some(rolled_back_at) = &run.rolled_back_at {
        bail!(
            "Land run {} was rolled back at {rolled_back_at}; its merged PRs have revert PRs. Run `knit land apply` to start a new landing instead.",
            run.id
        );
    }
    let plan_path = resolve_stored_path(&active.root, &run.plan_path);
    let plan: LandPlan = read_json(&plan_path)?;
    validate::validate_plan_for_bundle(&active, &plan)?;
    validate::preflight_required_checks(&active, &plan.require_checks, skip_checks)?;
    let order = validate::ordered_step_ids(&plan.steps)?;
    validate::preflight_publications(&active, &plan, Some(&run))?;
    run.status = LandStatus::Running;
    run.updated_at = now_iso();
    write_json(&path, &run)?;
    execute::execute_run(&mut active, &plan, &order, &mut run, &path)?;
    crate::commands::remote::sync_bundle_to_remote_if_enabled(remote, no_remote)?;
    Ok(())
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
    display::print_plan_landing_targets(&active, &plan);
    for step in &plan.steps {
        display::print_planned_step(&active, step);
    }
    Ok(())
}

fn archive_landed_bundle(active: &mut ActiveBundle, keep_worktrees: bool) -> Result<usize> {
    let summary = crate::commands::bundle::archive_active_bundle(
        active,
        Some("landed".to_string()),
        keep_worktrees,
        false,
    )?;
    save_active_bundle(active)?;
    crate::commands::bundle::clear_workspace_active_if_matches(&active.root, &active.bundle.id)?;
    Ok(summary.removed_worktrees)
}

fn print_landed_summary(bundle_id: &str, removed_worktrees: usize, keep_worktrees: bool) {
    if keep_worktrees {
        println!(
            "landed {}; kept generated worktrees (--keep-worktrees)",
            out::node(bundle_id)
        );
    } else {
        println!(
            "landed {}; removed {} worktree(s) (keep with --keep-worktrees)",
            out::node(bundle_id),
            removed_worktrees
        );
    }
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

/// The check names landing requires for this bundle: the plan file's
/// `requireChecks` when a plan exists, else the project landing template's.
pub(super) fn required_check_names(active: &ActiveBundle) -> Vec<String> {
    if let Ok(path) = resolve_land_plan_path(active, None) {
        if path.exists() {
            if let Ok(plan) = read_json::<LandPlan>(&path) {
                return plan.require_checks;
            }
        }
    }
    crate::store::load_config(&active.root)
        .ok()
        .and_then(|config| active.bundle.project_id.clone().or(config.active_project))
        .and_then(|project_id| {
            read_json::<crate::model::KnitProject>(&crate::store::project_path(
                &active.root,
                &project_id,
            ))
            .ok()
        })
        .and_then(|project| project.landing)
        .map(|landing| landing.require_checks)
        .unwrap_or_default()
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
        .find_map(|repo| {
            providers::for_repo(repo)
                .ok()
                .map(|forge| forge.id().to_string())
        })
        .unwrap_or_else(|| DEFAULT_LAND_PROVIDER.to_string())
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

fn ensure_open_for_retarget(repo_id: &str, pr: &PullRequest) -> Result<()> {
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
        .replace([':', '.'], "")
}

#[cfg(test)]
mod tests {
    use super::execute::{ensure_needs_succeeded, step_waves};
    use super::plan::default_deployment_needs;
    use super::validate::ordered_step_ids;
    use super::*;
    use crate::model::SCHEMA_VERSION;
    use std::collections::BTreeMap;

    fn step(id: &str, needs: &[&str]) -> LandStep {
        LandStep {
            id: id.to_string(),
            step_type: LandStepKind::Run,
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
    fn rejects_unknown_merge_methods_at_parse() {
        assert_eq!(
            serde_json::from_str::<crate::model::MergeMethod>("\"squash\"").unwrap(),
            crate::model::MergeMethod::Squash
        );
        assert!(
            serde_json::from_str::<crate::model::MergeMethod>("\"octopus\"")
                .unwrap_err()
                .to_string()
                .contains("expected one of")
        );
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
            status: LandStatus::Running,
            created_at: now_iso(),
            updated_at: now_iso(),
            rolled_back_at: None,
            steps: vec![LandRunStep {
                id: "a".to_string(),
                step_type: LandStepKind::Run,
                status: LandStatus::Failed,
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
            status: LandStatus::Succeeded,
            created_at: now_iso(),
            updated_at: now_iso(),
            rolled_back_at: None,
            steps: Vec::new(),
        };
        let text = serde_json::to_string_pretty(&run).unwrap();
        std::fs::write(path, format!("{text}\n")).unwrap();
    }
}
