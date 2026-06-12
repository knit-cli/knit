//! `knit merge` — integrate a source (bundle or ref) into a branch or bundle
//! target. This root holds the public entrypoints, the run/step data types, and
//! the shared locking and git helpers; the work lives in submodules:
//!
//! - [`run`] plans and applies the merge run (continue/abort/rollback)
//! - [`report`] `merge status`/`show` and pushing a recorded run

mod report;
mod run;

use crate::checkout::is_in_place;
use crate::git::{current_branch, git_output, git_output_optional, ref_exists};
use crate::ids::slugify;
use crate::model::{ChangeGroup, RepoEntry};
use crate::output as out;
use crate::store::{acquire_named_lock, bundle_path, find_knit_root, read_json, KnitLock};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MERGE_RUN_KIND: &str = "KnitMergeRun";

/// Status of a whole merge run. Serialized snake_case to match the recorded
/// run artifacts (`running`, `push_failed`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MergeRunStatus {
    Running,
    Succeeded,
    Conflicted,
    Aborted,
    PushFailed,
}

impl MergeRunStatus {
    fn as_str(self) -> &'static str {
        match self {
            MergeRunStatus::Running => "running",
            MergeRunStatus::Succeeded => "succeeded",
            MergeRunStatus::Conflicted => "conflicted",
            MergeRunStatus::Aborted => "aborted",
            MergeRunStatus::PushFailed => "push_failed",
        }
    }
}

impl std::fmt::Display for MergeRunStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

/// Status of one repo step within a merge run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MergeStepStatus {
    Pending,
    Succeeded,
    Conflicted,
    Aborted,
}

impl MergeStepStatus {
    fn as_str(self) -> &'static str {
        match self {
            MergeStepStatus::Pending => "pending",
            MergeStepStatus::Succeeded => "succeeded",
            MergeStepStatus::Conflicted => "conflicted",
            MergeStepStatus::Aborted => "aborted",
        }
    }
}

impl std::fmt::Display for MergeStepStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

/// What kind of target a step merges into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MergeTargetKind {
    Branch,
    Bundle,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeRun {
    schema_version: String,
    kind: String,
    id: String,
    source: String,
    into: String,
    manual: bool,
    status: MergeRunStatus,
    created_at: String,
    updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_node_id: Option<String>,
    #[serde(default)]
    fetch_requested: bool,
    #[serde(default)]
    push_requested: bool,
    #[serde(default)]
    set_upstream: bool,
    steps: Vec<MergeRunStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeRunStep {
    repo_id: String,
    repo_path: String,
    source_ref: String,
    target: String,
    target_kind: MergeTargetKind,
    checkout_path: String,
    before_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after_sha: Option<String>,
    status: MergeStepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pushed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pushed_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    push_remote: Option<String>,
}

struct SourcePlan {
    label: String,
    bundle_id: Option<String>,
    repos: Vec<RepoEntry>,
    refs_by_repo: BTreeMap<String, String>,
}

enum TargetPlan {
    Branch {
        label: String,
        branch: String,
    },
    Bundle {
        label: String,
        bundle_id: String,
        bundle: ChangeGroup,
    },
}

impl TargetPlan {
    fn label(&self) -> &str {
        match self {
            TargetPlan::Branch { label, .. } | TargetPlan::Bundle { label, .. } => label,
        }
    }

    fn kind(&self) -> MergeTargetKind {
        match self {
            TargetPlan::Branch { .. } => MergeTargetKind::Branch,
            TargetPlan::Bundle { .. } => MergeTargetKind::Bundle,
        }
    }

    fn bundle_id(&self) -> Option<&str> {
        match self {
            TargetPlan::Branch { .. } => None,
            TargetPlan::Bundle { bundle_id, .. } => Some(bundle_id),
        }
    }

    fn step_target_for(&self, repo_id: &str) -> Result<String> {
        match self {
            TargetPlan::Branch { branch, .. } => Ok(branch.clone()),
            TargetPlan::Bundle { bundle, .. } => {
                let repo = bundle
                    .repos
                    .iter()
                    .find(|repo| repo.id == repo_id)
                    .with_context(|| format!("Target bundle has no repo {repo_id}."))?;
                repo.feature_branch
                    .clone()
                    .with_context(|| format!("{repo_id}: target bundle has no feature branch."))
            }
        }
    }
}

pub fn merge_command(
    source: Option<&str>,
    into: Option<&str>,
    manual: bool,
    fetch: bool,
    push: bool,
    set_upstream: bool,
    run: Option<&str>,
    repos: &[String],
    continue_run: bool,
    abort: bool,
) -> Result<()> {
    let root = current_knit_root()?;
    if into.is_none() {
        match source {
            Some("status") => return report::show_merge_status(&root, run),
            Some("show") => return report::show_merge_run_json(&root, run),
            Some("push") => {
                return report::push_recorded_merge_run(&root, run, repos, set_upstream)
            }
            _ => {}
        }
    }

    let selected_modes =
        usize::from(source.is_some()) + usize::from(continue_run) + usize::from(abort);
    if selected_modes != 1 {
        bail!("Use `knit merge <source> --into <target>`, `knit merge --continue`, or `knit merge --abort`.");
    }
    if source.is_some() && into.is_none() {
        bail!("`knit merge <source>` requires --into <target>.");
    }
    if source.is_none() && (into.is_some() || manual) {
        bail!("--into and --manual are only valid when starting a merge run.");
    }

    if abort {
        return run::abort_latest_merge(&root);
    }
    if continue_run {
        return run::continue_latest_merge(&root);
    }

    run::start_merge(
        &root,
        source.expect("validated"),
        into.expect("validated"),
        manual,
        fetch,
        push,
        set_upstream,
    )
}

// ---------------------------------------------------------------------------
// Shared run lookup, locking, and git helpers used across the merge submodules.
// ---------------------------------------------------------------------------

fn latest_merge_run(root: &Path, statuses: &[MergeRunStatus]) -> Result<(PathBuf, MergeRun)> {
    let runs_dir = root.join(".knit/merge-runs");
    if !runs_dir.exists() {
        bail!("No merge runs found.");
    }

    let mut candidates = Vec::new();
    for entry in
        fs::read_dir(&runs_dir).with_context(|| format!("failed to read {}", runs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let run: MergeRun = read_json(&path)?;
        if statuses.is_empty() || statuses.contains(&run.status) {
            candidates.push((path, run));
        }
    }

    candidates
        .into_iter()
        .max_by(|(_, left), (_, right)| left.created_at.cmp(&right.created_at))
        .context("No matching merge run found.")
}

fn acquire_merge_locks(
    root: &Path,
    source: &SourcePlan,
    target: &TargetPlan,
) -> Result<Vec<KnitLock>> {
    let mut locks = Vec::new();
    if let Some(bundle_id) = target.bundle_id() {
        locks.push(acquire_named_lock(root, bundle_id)?);
    }
    for repo in &source.repos {
        locks.push(acquire_named_lock(
            root,
            &format!("merge-{}-{}", slugify(target.label()), slugify(&repo.id)),
        )?);
    }
    Ok(locks)
}

fn acquire_run_locks(root: &Path, run: &MergeRun) -> Result<Vec<KnitLock>> {
    let mut locks = Vec::new();
    if let Some(bundle_id) = &run.target_bundle_id {
        locks.push(acquire_named_lock(root, bundle_id)?);
    }
    for step in &run.steps {
        if matches!(
            step.status,
            MergeStepStatus::Succeeded | MergeStepStatus::Conflicted | MergeStepStatus::Pending
        ) {
            locks.push(acquire_named_lock(
                root,
                &format!("merge-{}-{}", slugify(&run.into), slugify(&step.repo_id)),
            )?);
        }
    }
    Ok(locks)
}

fn load_bundle(root: &Path, bundle_id: &str) -> Result<ChangeGroup> {
    read_json(&bundle_path(root, bundle_id))
}

fn merge_run_path(root: &Path, run_id: &str) -> PathBuf {
    root.join(".knit/merge-runs").join(format!("{run_id}.json"))
}

fn current_knit_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    find_knit_root(&cwd)
        .context("No Knit workspace found. Run `knit bundle \"feature title\"` first.")
}

fn checkout_path_for(root: &Path, repo: &RepoEntry) -> Option<PathBuf> {
    if let Some(path) = &repo.worktree_path {
        let path = resolve_stored_path(root, path);
        return path.exists().then_some(path);
    }

    if is_in_place(repo) {
        let path = PathBuf::from(&repo.path);
        return path.exists().then_some(path);
    }

    None
}

fn resolve_stored_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn ensure_checkout_on_branch(repo: &RepoEntry, checkout: &Path) -> Result<()> {
    let Some(expected) = &repo.feature_branch else {
        bail!("{}: target bundle has no feature branch recorded.", repo.id);
    };
    let actual = current_branch(checkout)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != *expected {
        bail!(
            "{}: target checkout is on {}, expected {}.",
            repo.id,
            out::branch(actual),
            out::branch(expected)
        );
    }
    Ok(())
}

fn ensure_ref_exists(cwd: &Path, reference: &str) -> Result<()> {
    if ref_exists(cwd, reference) {
        Ok(())
    } else {
        bail!("ref {reference} does not exist")
    }
}

fn merge_in_progress(cwd: &Path) -> bool {
    git_output_optional(cwd, ["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .map(|output| output.is_some())
        .unwrap_or(false)
}

fn has_unmerged_paths(cwd: &Path) -> bool {
    git_output(cwd, ["diff", "--name-only", "--diff-filter=U"])
        .map(|output| !output.trim().is_empty())
        .unwrap_or(false)
}

fn abort_merge_if_needed(cwd: &Path) {
    if merge_in_progress(cwd) {
        let _ = git_output(cwd, ["merge", "--abort"]);
    }
}

fn hard_reset(cwd: &Path, reference: &str) -> Result<()> {
    git_output(cwd, ["reset", "--hard", reference]).map(|_| ())
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn short_or_dash(sha: &str) -> &str {
    if sha.is_empty() {
        "-"
    } else {
        short_sha(sha)
    }
}
