use crate::advice;
use crate::checkout::is_in_place;
use crate::git::{
    branch_exists, current_branch, git_output, git_output_optional, is_ancestor, is_git_worktree,
    ref_exists, resolve_base_ref, rev_list, rev_parse,
};
use crate::ids::{node_id, slugify};
use crate::model::{
    BundleNode, ChangeGroup, RepoChange, RepoEntry, CHECKOUT_MODE_IN_PLACE, CHECKOUT_MODE_WORKTREE,
    SCHEMA_VERSION,
};
use crate::output as out;
use crate::store::{
    acquire_named_lock, bundle_exists, bundle_path, find_knit_root, load_active_bundle,
    load_config, read_json, relative_path_for_storage, save_config, set_agent_active_bundle,
    write_json, ActiveBundle, KnitLock,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

const MERGE_RUN_KIND: &str = "KnitMergeRun";
const STEP_PENDING: &str = "pending";
const STEP_SUCCEEDED: &str = "succeeded";
const STEP_CONFLICTED: &str = "conflicted";
const STEP_ABORTED: &str = "aborted";
const RUN_RUNNING: &str = "running";
const RUN_SUCCEEDED: &str = "succeeded";
const RUN_CONFLICTED: &str = "conflicted";
const RUN_ABORTED: &str = "aborted";
const RUN_PUSH_FAILED: &str = "push_failed";
const TARGET_BRANCH: &str = "branch";
const TARGET_BUNDLE: &str = "bundle";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeRun {
    schema_version: String,
    kind: String,
    id: String,
    source: String,
    into: String,
    manual: bool,
    status: String,
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
    target_kind: String,
    checkout_path: String,
    before_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after_sha: Option<String>,
    status: String,
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
            Some("status") => return show_merge_status(&root, run),
            Some("show") => return show_merge_run_json(&root, run),
            Some("push") => return push_recorded_merge_run(&root, run, repos, set_upstream),
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
        return abort_latest_merge(&root);
    }
    if continue_run {
        return continue_latest_merge(&root);
    }

    start_merge(
        &root,
        source.expect("validated"),
        into.expect("validated"),
        manual,
        fetch,
        push,
        set_upstream,
    )
}

pub fn create_compat_bundle(
    sources: &[String],
    title: Option<&str>,
    project: Option<&str>,
    all_repos: bool,
    materialize: bool,
    in_place: bool,
    force: bool,
) -> Result<()> {
    if sources.is_empty() {
        bail!("Pass at least one source bundle for a compatibility bundle.");
    }

    let root = current_knit_root()?;
    let source_bundles = sources
        .iter()
        .map(|source| load_bundle(&root, &slugify(source)))
        .collect::<Result<Vec<_>>>()?;
    let title = title
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("compat {}", sources.join(" ")));

    if let Some(project_id) = project {
        let repo_ids = if all_repos {
            Vec::new()
        } else {
            union_source_repo_ids(&source_bundles)
        };
        return crate::commands::init::start_bundle(
            &title,
            Some(project_id),
            &repo_ids,
            all_repos,
            materialize,
            in_place,
            force,
            false,
        );
    }

    if all_repos {
        bail!("--all-repos is only valid with --project.");
    }

    create_compat_bundle_from_sources(&root, &title, &source_bundles, materialize, in_place, force)
}

fn start_merge(
    root: &Path,
    source: &str,
    into: &str,
    manual: bool,
    fetch: bool,
    push: bool,
    set_upstream: bool,
) -> Result<()> {
    let source_plan = resolve_source_plan(root, source)?;
    let target_plan = resolve_target_plan(root, into)?;
    if push && target_plan.bundle_id().is_some() {
        bail!("`knit merge --push` only supports branch targets. Use `knit push --bundle {into}` for bundle targets.");
    }
    let _locks = acquire_merge_locks(root, &source_plan, &target_plan)?;
    let run_id = node_id("merge");
    let run_path = merge_run_path(root, &run_id);
    let now = now_iso();
    let mut run = MergeRun {
        schema_version: SCHEMA_VERSION.to_string(),
        kind: MERGE_RUN_KIND.to_string(),
        id: run_id.clone(),
        source: source_plan.label.clone(),
        into: target_plan.label().to_string(),
        manual,
        status: RUN_RUNNING.to_string(),
        created_at: now.clone(),
        updated_at: now,
        source_bundle_id: source_plan.bundle_id.clone(),
        target_bundle_id: target_plan.bundle_id().map(ToString::to_string),
        target_node_id: None,
        fetch_requested: fetch,
        push_requested: push,
        set_upstream,
        steps: Vec::new(),
    };

    fs::create_dir_all(root.join(".knit/merge-runs"))
        .context("failed to create .knit/merge-runs")?;
    fs::create_dir_all(root.join(".knit/merge-worktrees"))
        .context("failed to create .knit/merge-worktrees")?;

    for repo in &source_plan.repos {
        let source_ref = source_plan
            .refs_by_repo
            .get(&repo.id)
            .with_context(|| format!("{}: no source ref resolved", repo.id))?;
        let step = prepare_merge_step(root, &target_plan, repo, source_ref, fetch)?;
        run.steps.push(step);
    }
    write_json(&run_path, &run)?;

    apply_pending_merge_steps(root, &run_path, &mut run, target_plan.bundle_id().is_some())
}

fn apply_pending_merge_steps(
    root: &Path,
    run_path: &Path,
    run: &mut MergeRun,
    target_bundle_lock_held: bool,
) -> Result<()> {
    let mut failures = Vec::new();
    for index in 0..run.steps.len() {
        if run.steps[index].status != STEP_PENDING {
            continue;
        }

        let result = apply_merge_step(root, &mut run.steps[index], run.manual);
        run.updated_at = now_iso();
        match result {
            Ok(()) => {
                println!(
                    "{}: {} {} into {}",
                    out::repo(&run.steps[index].repo_id),
                    out::movement("merged"),
                    out::branch(&run.steps[index].source_ref),
                    out::branch(&run.steps[index].target)
                );
                write_json(run_path, run)?;
            }
            Err(MergeStepFailure::Conflict(message)) if run.manual => {
                run.status = RUN_CONFLICTED.to_string();
                run.steps[index].status = STEP_CONFLICTED.to_string();
                run.steps[index].message = Some(message);
                write_json(run_path, run)?;
                println!(
                    "{}: {} {}",
                    out::repo(&run.steps[index].repo_id),
                    out::danger("conflict"),
                    out::path(&run.steps[index].checkout_path)
                );
                advice::print(
                    root,
                    format!(
                        "resolve conflicts in {}, then run `knit merge --continue` or `knit merge --abort`.",
                        run.steps[index].checkout_path
                    ),
                );
                bail!(
                    "Merge stopped for manual conflict resolution. Resolve and commit in {}, then run `knit merge --continue`, or run `knit merge --abort`.",
                    run.steps[index].checkout_path
                );
            }
            Err(MergeStepFailure::Conflict(message)) => {
                run.steps[index].status = STEP_ABORTED.to_string();
                run.steps[index].message = Some(message.clone());
                rollback_merge_run(root, run)?;
                run.status = RUN_ABORTED.to_string();
                run.updated_at = now_iso();
                write_json(run_path, run)?;
                failures.push(format!("{}: {message}", run.steps[index].repo_id));
                break;
            }
            Err(MergeStepFailure::Fatal(error)) => {
                run.steps[index].status = STEP_ABORTED.to_string();
                run.steps[index].message = Some(error.clone());
                rollback_merge_run(root, run)?;
                run.status = RUN_ABORTED.to_string();
                run.updated_at = now_iso();
                write_json(run_path, run)?;
                failures.push(format!("{}: {error}", run.steps[index].repo_id));
                break;
            }
        }
    }

    if !failures.is_empty() {
        bail!(
            "Merge aborted and this run was rolled back:\n{}",
            failures.join("\n")
        );
    }

    if run.steps.iter().all(|step| step.status == STEP_SUCCEEDED) {
        finalize_target_bundle(root, run, target_bundle_lock_held)?;
        if run.push_requested {
            if let Err(error) = push_merge_run_steps(root, run, &[], run.set_upstream) {
                run.status = RUN_PUSH_FAILED.to_string();
                run.updated_at = now_iso();
                write_json(run_path, run)?;
                bail!("Merge succeeded locally, but push failed:\n{error:#}");
            }
        }
        run.status = RUN_SUCCEEDED.to_string();
        run.updated_at = now_iso();
        write_json(run_path, run)?;
        println!(
            "{} {} into {}",
            out::heading("Merged:"),
            out::branch(&run.source),
            out::branch(&run.into)
        );
    }

    Ok(())
}

enum MergeStepFailure {
    Conflict(String),
    Fatal(String),
}

fn apply_merge_step(
    root: &Path,
    step: &mut MergeRunStep,
    manual: bool,
) -> std::result::Result<(), MergeStepFailure> {
    let checkout = resolve_stored_path(root, &step.checkout_path);
    let status = git_output(&checkout, ["status", "--porcelain"])
        .map_err(|error| MergeStepFailure::Fatal(format!("{error:#}")))?;
    if !status.trim().is_empty() {
        return Err(MergeStepFailure::Fatal(format!(
            "target checkout is not clean: {}",
            checkout.display()
        )));
    }

    let merge_result = git_output(
        &checkout,
        [
            OsString::from("merge"),
            OsString::from("--no-ff"),
            OsString::from("--no-edit"),
            OsString::from(&step.source_ref),
        ],
    );

    match merge_result {
        Ok(_) => {
            step.after_sha = Some(
                rev_parse(&checkout, "HEAD")
                    .map_err(|error| MergeStepFailure::Fatal(format!("{error:#}")))?,
            );
            step.status = STEP_SUCCEEDED.to_string();
            Ok(())
        }
        Err(error) if merge_in_progress(&checkout) || has_unmerged_paths(&checkout) => {
            if !manual {
                abort_merge_if_needed(&checkout);
                hard_reset(&checkout, &step.before_sha);
            }
            Err(MergeStepFailure::Conflict(format!("{error:#}")))
        }
        Err(error) => {
            abort_merge_if_needed(&checkout);
            hard_reset(&checkout, &step.before_sha);
            Err(MergeStepFailure::Fatal(format!("{error:#}")))
        }
    }
}

fn continue_latest_merge(root: &Path) -> Result<()> {
    let (run_path, mut run) = latest_merge_run(root, &[RUN_CONFLICTED])?;
    let _locks = acquire_run_locks(root, &run)?;
    let Some(index) = run
        .steps
        .iter()
        .position(|step| step.status == STEP_CONFLICTED)
    else {
        bail!("Latest conflicted merge run has no conflicted step.");
    };

    let checkout = resolve_stored_path(root, &run.steps[index].checkout_path);
    if has_unmerged_paths(&checkout) {
        bail!(
            "{}: unresolved conflicts remain. Resolve them before running `knit merge --continue`.",
            run.steps[index].repo_id
        );
    }

    if merge_in_progress(&checkout) {
        git_output(&checkout, ["commit", "--no-edit"]).with_context(|| {
            format!(
                "{}: failed to commit resolved merge in {}",
                run.steps[index].repo_id,
                checkout.display()
            )
        })?;
    }

    let status = git_output(&checkout, ["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        bail!(
            "{}: target checkout still has uncommitted changes. Commit or clean them before continuing.",
            run.steps[index].repo_id
        );
    }

    let after_sha = rev_parse(&checkout, "HEAD")?;
    if after_sha == run.steps[index].before_sha {
        bail!(
            "{}: HEAD did not move. Complete the merge commit before continuing.",
            run.steps[index].repo_id
        );
    }

    run.steps[index].after_sha = Some(after_sha);
    run.steps[index].status = STEP_SUCCEEDED.to_string();
    run.steps[index].message = None;
    println!(
        "{}: {} {} into {}",
        out::repo(&run.steps[index].repo_id),
        out::movement("resolved"),
        out::branch(&run.steps[index].source_ref),
        out::branch(&run.steps[index].target)
    );
    run.status = RUN_RUNNING.to_string();
    run.updated_at = now_iso();
    write_json(&run_path, &run)?;
    let target_bundle_lock_held = run.target_bundle_id.is_some();
    apply_pending_merge_steps(root, &run_path, &mut run, target_bundle_lock_held)
}

fn abort_latest_merge(root: &Path) -> Result<()> {
    let (run_path, mut run) = latest_merge_run(root, &[RUN_CONFLICTED, RUN_RUNNING])?;
    let _locks = acquire_run_locks(root, &run)?;
    rollback_merge_run(root, &mut run)?;
    run.status = RUN_ABORTED.to_string();
    run.updated_at = now_iso();
    write_json(&run_path, &run)?;
    println!("{} {}", out::heading("Aborted:"), out::branch(&run.id));
    Ok(())
}

fn rollback_merge_run(root: &Path, run: &mut MergeRun) -> Result<()> {
    for step in run.steps.iter_mut().rev() {
        if step.status != STEP_SUCCEEDED
            && step.status != STEP_CONFLICTED
            && step.status != STEP_ABORTED
        {
            continue;
        }
        let checkout = resolve_stored_path(root, &step.checkout_path);
        abort_merge_if_needed(&checkout);
        hard_reset(&checkout, &step.before_sha);
        step.after_sha = None;
        step.status = STEP_ABORTED.to_string();
    }
    Ok(())
}

fn prepare_merge_step(
    root: &Path,
    target: &TargetPlan,
    source_repo: &RepoEntry,
    source_ref: &str,
    fetch: bool,
) -> Result<MergeRunStep> {
    let checkout = match target {
        TargetPlan::Branch { branch, .. } => {
            prepare_branch_checkout(root, source_repo, branch, fetch)?
        }
        TargetPlan::Bundle { bundle, .. } => {
            let target_repo = bundle
                .repos
                .iter()
                .find(|repo| repo.id == source_repo.id)
                .with_context(|| {
                    format!(
                        "Target bundle {} does not include repo {}.",
                        bundle.id, source_repo.id
                    )
                })?;
            let checkout = checkout_path_for(root, target_repo).with_context(|| {
                format!(
                    "{}: target bundle checkout is not materialized. Run `knit worktree --bundle {}`.",
                    target_repo.id, bundle.id
                )
            })?;
            ensure_checkout_on_branch(target_repo, &checkout)?;
            checkout
        }
    };

    ensure_ref_exists(&checkout, source_ref)
        .with_context(|| format!("{}: source ref {source_ref} was not found", source_repo.id))?;
    let status = git_output(&checkout, ["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        bail!(
            "{}: target checkout is not clean at {}.",
            source_repo.id,
            checkout.display()
        );
    }
    let before_sha = rev_parse(&checkout, "HEAD")?;
    Ok(MergeRunStep {
        repo_id: source_repo.id.clone(),
        repo_path: source_repo.path.clone(),
        source_ref: source_ref.to_string(),
        target: target.step_target_for(&source_repo.id)?,
        target_kind: target.kind().to_string(),
        checkout_path: relative_path_for_storage(root, &checkout),
        before_sha,
        after_sha: None,
        status: STEP_PENDING.to_string(),
        message: None,
        pushed_at: None,
        pushed_sha: None,
        push_remote: None,
    })
}

fn prepare_branch_checkout(
    root: &Path,
    repo: &RepoEntry,
    branch: &str,
    fetch: bool,
) -> Result<PathBuf> {
    let repo_root = PathBuf::from(&repo.path);
    if fetch {
        fetch_target_branch(&repo_root, &repo.id, branch)?;
    }
    let worktree_path = root
        .join(".knit/merge-worktrees")
        .join(slugify(branch))
        .join(&repo.id);

    if worktree_path.exists() {
        if !is_git_worktree(&worktree_path) {
            bail!(
                "{}: {} exists but is not a git worktree.",
                repo.id,
                worktree_path.display()
            );
        }
        let current =
            current_branch(&worktree_path)?.unwrap_or_else(|| "(detached HEAD)".to_string());
        if current != branch {
            bail!(
                "{}: merge checkout {} is on {}, expected {}.",
                repo.id,
                worktree_path.display(),
                out::branch(current),
                out::branch(branch)
            );
        }
        if fetch {
            fast_forward_target(&worktree_path, &repo.id, branch)?;
        }
        return Ok(worktree_path);
    }

    if let Some(parent) = worktree_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if branch_exists(&repo_root, branch) {
        match git_output(
            &repo_root,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                worktree_path.as_os_str().to_os_string(),
                OsString::from(branch),
            ],
        ) {
            Ok(_) => {
                if fetch {
                    fast_forward_target(&worktree_path, &repo.id, branch)?;
                }
                return Ok(worktree_path);
            }
            Err(error) => {
                if current_branch(&repo_root)?.as_deref() == Some(branch) {
                    if fetch {
                        fast_forward_target(&repo_root, &repo.id, branch)?;
                    }
                    return Ok(repo_root);
                }
                return Err(error).with_context(|| {
                    format!("{}: failed to create merge worktree for {branch}", repo.id)
                });
            }
        }
    }

    let base_ref = resolve_base_ref(&repo_root, branch);
    if !ref_exists(&repo_root, &base_ref) {
        bail!(
            "{}: target branch {} does not exist locally or as origin/{}.",
            repo.id,
            out::branch(branch),
            branch
        );
    }
    git_output(
        &repo_root,
        [
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-b"),
            OsString::from(branch),
            worktree_path.as_os_str().to_os_string(),
            OsString::from(base_ref),
        ],
    )
    .with_context(|| format!("{}: failed to create merge worktree for {branch}", repo.id))?;
    if fetch {
        fast_forward_target(&worktree_path, &repo.id, branch)?;
    }
    Ok(worktree_path)
}

fn fetch_target_branch(repo_root: &Path, repo_id: &str, branch: &str) -> Result<()> {
    let refspec = format!("{branch}:refs/remotes/origin/{branch}");
    git_output(repo_root, ["fetch", "origin", refspec.as_str()])
        .with_context(|| format!("{repo_id}: failed to fetch origin/{branch}"))?;
    Ok(())
}

fn fast_forward_target(checkout: &Path, repo_id: &str, branch: &str) -> Result<()> {
    let remote = format!("origin/{branch}");
    let head = rev_parse(checkout, "HEAD")?;
    let fetched = rev_parse(checkout, &remote)?;
    if head != fetched {
        if !is_ancestor(checkout, &head, &fetched) {
            bail!("{repo_id}: {branch} has local commits not in origin/{branch}.");
        }
        git_output(checkout, ["reset", "--hard", remote.as_str()]).with_context(|| {
            format!("{repo_id}: failed to fast-forward {branch} from origin/{branch}")
        })?;
    }
    Ok(())
}

fn resolve_source_plan(root: &Path, source: &str) -> Result<SourcePlan> {
    let source_id = slugify(source);
    if bundle_exists(root, &source_id) {
        let bundle = load_bundle(root, &source_id)?;
        if bundle.repos.is_empty() {
            bail!("Source bundle {} has no repos.", out::repo(&source_id));
        }
        let refs_by_repo = bundle
            .repos
            .iter()
            .map(|repo| {
                let feature_branch = repo.feature_branch.clone().with_context(|| {
                    format!(
                        "{}: source bundle has no feature branch. Run `knit worktree --bundle {}`.",
                        repo.id, bundle.id
                    )
                })?;
                Ok((repo.id.clone(), feature_branch))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        return Ok(SourcePlan {
            label: bundle.id.clone(),
            bundle_id: Some(bundle.id.clone()),
            repos: bundle.repos,
            refs_by_repo,
        });
    }

    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos to apply source ref {source} across.");
    }
    let refs_by_repo = active
        .bundle
        .repos
        .iter()
        .map(|repo| (repo.id.clone(), source.to_string()))
        .collect();
    Ok(SourcePlan {
        label: source.to_string(),
        bundle_id: None,
        repos: active.bundle.repos,
        refs_by_repo,
    })
}

fn resolve_target_plan(root: &Path, into: &str) -> Result<TargetPlan> {
    let target_id = slugify(into);
    if bundle_exists(root, &target_id) {
        let path = bundle_path(root, &target_id);
        let bundle: ChangeGroup = read_json(&path)?;
        return Ok(TargetPlan::Bundle {
            label: bundle.id.clone(),
            bundle_id: bundle.id.clone(),
            bundle,
        });
    }

    Ok(TargetPlan::Branch {
        label: into.to_string(),
        branch: into.to_string(),
    })
}

impl TargetPlan {
    fn label(&self) -> &str {
        match self {
            TargetPlan::Branch { label, .. } | TargetPlan::Bundle { label, .. } => label,
        }
    }

    fn kind(&self) -> &str {
        match self {
            TargetPlan::Branch { .. } => TARGET_BRANCH,
            TargetPlan::Bundle { .. } => TARGET_BUNDLE,
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

fn finalize_target_bundle(
    root: &Path,
    run: &mut MergeRun,
    target_bundle_lock_held: bool,
) -> Result<()> {
    let Some(target_bundle_id) = &run.target_bundle_id else {
        return Ok(());
    };
    if run.target_node_id.is_some() {
        return Ok(());
    }
    let _lock = if target_bundle_lock_held {
        None
    } else {
        Some(acquire_named_lock(root, target_bundle_id)?)
    };
    let target_path = bundle_path(root, target_bundle_id);
    let mut bundle: ChangeGroup = read_json(&target_path)?;
    let mut changes = Vec::new();
    for step in &run.steps {
        let Some(after_sha) = &step.after_sha else {
            continue;
        };
        let checkout = resolve_stored_path(root, &step.checkout_path);
        let commits = rev_list(&checkout, &step.before_sha, after_sha).unwrap_or_default();
        if let Some(repo) = bundle.repos.iter_mut().find(|repo| repo.id == step.repo_id) {
            repo.head_sha = Some(after_sha.clone());
        }
        changes.push(RepoChange {
            repo_id: step.repo_id.clone(),
            movement: "advanced".to_string(),
            before_sha: Some(step.before_sha.clone()),
            after_sha: after_sha.clone(),
            commits,
            dropped_commits: Vec::new(),
        });
    }
    if changes.is_empty() {
        return Ok(());
    }
    let now = now_iso();
    let node_id = node_id("merge");
    bundle.nodes.push(BundleNode::git_observed(
        node_id.clone(),
        now.clone(),
        changes,
    ));
    bundle.head_node_id = bundle.nodes.last().map(|node| node.id.clone());
    bundle.updated_at = now;
    write_json(&target_path, &bundle)?;
    run.target_node_id = Some(node_id);
    Ok(())
}

fn create_compat_bundle_from_sources(
    root: &Path,
    title: &str,
    source_bundles: &[ChangeGroup],
    materialize: bool,
    in_place: bool,
    force: bool,
) -> Result<()> {
    let bundle_id = slugify(title);
    let knit_dir = root.join(".knit");
    let bundle_dir = knit_dir.join("bundles");
    let worktree_dir = knit_dir.join("worktrees").join(&bundle_id);
    let path = bundle_path(root, &bundle_id);
    if path.exists() && !force {
        bail!(
            "Bundle {} already exists. Use --force to overwrite it.",
            path.display()
        );
    }

    fs::create_dir_all(&bundle_dir).context("failed to create .knit/bundles")?;
    fs::create_dir_all(&worktree_dir).context("failed to create .knit/worktrees")?;

    let now = now_iso();
    let checkout_mode = if in_place {
        CHECKOUT_MODE_IN_PLACE
    } else {
        CHECKOUT_MODE_WORKTREE
    };
    let mut bundle = ChangeGroup::new(bundle_id.clone(), title.to_string(), now.clone());
    bundle.repos = union_source_repos(source_bundles, checkout_mode)?;
    if !bundle.repos.is_empty() {
        bundle.nodes.push(BundleNode::repos_added(
            node_id("repo"),
            now.clone(),
            bundle.repos.iter().map(|repo| repo.id.clone()).collect(),
        ));
        bundle.head_node_id = bundle.nodes.last().map(|node| node.id.clone());
    }
    write_json(&path, &bundle)?;

    let current_agent = crate::store::current_agent_id();
    let mut config = load_config(root)?;
    if current_agent.is_none() || config.active_bundle.is_none() {
        config.active_bundle = Some(bundle_id.clone());
    }
    save_config(root, &config)?;
    if let Some(agent_id) = &current_agent {
        set_agent_active_bundle(root, agent_id, &bundle_id)?;
    }

    if materialize && !bundle.repos.is_empty() {
        let mut active = ActiveBundle::unlocked(root.to_path_buf(), path.clone(), bundle);
        let materialized = crate::commands::worktree::materialize_repos(&mut active, None)?;
        let now = now_iso();
        active.bundle.nodes.push(BundleNode::worktrees_materialized(
            node_id("worktree"),
            now.clone(),
            materialized,
        ));
        active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
        active.bundle.updated_at = now;
        write_json(&path, &active.bundle)?;
    }

    println!(
        "{} {}",
        out::heading("Compatibility bundle:"),
        out::path(path.display())
    );
    Ok(())
}

fn union_source_repo_ids(source_bundles: &[ChangeGroup]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut repo_ids = Vec::new();
    for bundle in source_bundles {
        for repo in &bundle.repos {
            if seen.insert(repo.id.clone()) {
                repo_ids.push(repo.id.clone());
            }
        }
    }
    repo_ids
}

fn union_source_repos(
    source_bundles: &[ChangeGroup],
    checkout_mode: &str,
) -> Result<Vec<RepoEntry>> {
    let mut repos: BTreeMap<String, RepoEntry> = BTreeMap::new();
    for bundle in source_bundles {
        for repo in &bundle.repos {
            if let Some(existing) = repos.get(&repo.id) {
                if existing.path != repo.path {
                    bail!(
                        "Repo id {} points at different paths in source bundles: {} and {}.",
                        repo.id,
                        existing.path,
                        repo.path
                    );
                }
                continue;
            }
            repos.insert(
                repo.id.clone(),
                RepoEntry {
                    id: repo.id.clone(),
                    path: repo.path.clone(),
                    remote: repo.remote.clone(),
                    base_branch: repo.base_branch.clone(),
                    checkout_mode: checkout_mode.to_string(),
                    base_sha: repo.base_sha.clone(),
                    feature_branch: None,
                    worktree_path: None,
                    head_sha: None,
                },
            );
        }
    }
    Ok(repos.into_values().collect())
}

fn latest_merge_run(root: &Path, statuses: &[&str]) -> Result<(PathBuf, MergeRun)> {
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
        if statuses.is_empty() || statuses.iter().any(|status| *status == run.status) {
            candidates.push((path, run));
        }
    }

    candidates
        .into_iter()
        .max_by(|(_, left), (_, right)| left.created_at.cmp(&right.created_at))
        .context("No matching merge run found.")
}

fn show_merge_status(root: &Path, run_selector: Option<&str>) -> Result<()> {
    let (path, run) = resolve_merge_run(root, run_selector, &[])?;
    println!(
        "{} {} {}",
        out::heading("Merge run"),
        out::node(&run.id),
        out::status(&run.status)
    );
    println!("{} {} -> {}", out::heading("Flow:"), run.source, run.into);
    println!(
        "{} {}",
        out::heading("Run file:"),
        out::path(path.display())
    );
    println!();
    println!(
        "{}  {}  {}  {}  {}  {}",
        out::header_field("repo", 14),
        out::header_field("target", 18),
        out::header_field("status", 12),
        out::header_field("before", 8),
        out::header_field("after", 8),
        out::heading("checkout")
    );
    for step in &run.steps {
        println!(
            "{}  {}  {}  {}  {}  {}",
            out::repo_field(&step.repo_id, 14),
            out::branch_field(&step.target, 18),
            out::status(&format!("{:<12}", step.status)),
            out::sha(short_or_dash(&step.before_sha)),
            out::sha(step.after_sha.as_deref().map(short_sha).unwrap_or("-")),
            out::path(&step.checkout_path)
        );
        if let Some(pushed_sha) = &step.pushed_sha {
            println!(
                "  pushed {} {}",
                out::sha(short_sha(pushed_sha)),
                step.pushed_at.as_deref().unwrap_or("")
            );
        } else if step.target_kind == TARGET_BRANCH && step.status == STEP_SUCCEEDED {
            println!("  {}", out::muted("not pushed"));
        }
        if let Some(message) = &step.message {
            println!(
                "  {}",
                out::danger(message.lines().next().unwrap_or(message))
            );
        }
    }
    if run.status == RUN_CONFLICTED {
        advice::print(
            root,
            "`knit merge --continue` after resolving conflicts, or `knit merge --abort`.",
        );
    }
    Ok(())
}

fn show_merge_run_json(root: &Path, run_selector: Option<&str>) -> Result<()> {
    let (_, run) = resolve_merge_run(root, run_selector, &[])?;
    println!("{}", serde_json::to_string_pretty(&run)?);
    Ok(())
}

fn push_recorded_merge_run(
    root: &Path,
    run_selector: Option<&str>,
    repos: &[String],
    set_upstream: bool,
) -> Result<()> {
    let (path, mut run) = resolve_merge_run(root, run_selector, &[RUN_SUCCEEDED, RUN_PUSH_FAILED])?;
    let _locks = acquire_run_locks(root, &run)?;
    push_merge_run_steps(root, &mut run, repos, set_upstream)?;
    run.status = RUN_SUCCEEDED.to_string();
    run.updated_at = now_iso();
    write_json(&path, &run)?;
    Ok(())
}

fn push_merge_run_steps(
    root: &Path,
    run: &mut MergeRun,
    repos: &[String],
    set_upstream: bool,
) -> Result<()> {
    let repo_filter = repos
        .iter()
        .map(|repo| slugify(repo))
        .collect::<BTreeSet<_>>();
    let mut failures = Vec::new();
    let mut eligible = 0usize;
    for step in &mut run.steps {
        if step.target_kind != TARGET_BRANCH {
            continue;
        }
        if !repo_filter.is_empty() && !repo_filter.contains(&step.repo_id) {
            continue;
        }
        eligible += 1;
        if step.status != STEP_SUCCEEDED {
            failures.push(format!(
                "{}: step is {}, expected succeeded",
                step.repo_id, step.status
            ));
            continue;
        }
        let Some(after_sha) = &step.after_sha else {
            failures.push(format!("{}: no afterSha recorded", step.repo_id));
            continue;
        };
        let checkout = resolve_stored_path(root, &step.checkout_path);
        let head = match rev_parse(&checkout, "HEAD") {
            Ok(head) => head,
            Err(error) => {
                failures.push(format!("{}: {error:#}", step.repo_id));
                continue;
            }
        };
        if head != *after_sha {
            failures.push(format!(
                "{}: checkout HEAD {} does not match merge run afterSha {}",
                step.repo_id,
                short_sha(&head),
                short_sha(after_sha)
            ));
            continue;
        }
        let mut args = vec![OsString::from("push")];
        if set_upstream {
            args.push(OsString::from("-u"));
        }
        args.push(OsString::from("origin"));
        args.push(OsString::from(&step.target));
        match git_output(&checkout, args) {
            Ok(_) => {
                let now = now_iso();
                step.pushed_at = Some(now);
                step.pushed_sha = Some(after_sha.clone());
                step.push_remote = Some("origin".to_string());
                println!(
                    "{}: {} origin/{} {}",
                    out::repo(&step.repo_id),
                    out::movement("pushed"),
                    step.target,
                    out::sha(short_sha(after_sha))
                );
            }
            Err(error) => failures.push(format!("{}: {error:#}", step.repo_id)),
        }
    }
    if !failures.is_empty() {
        bail!("merge push failed:\n{}", failures.join("\n"));
    }
    if eligible == 0 {
        bail!("Merge run has no branch-target steps to push.");
    }
    Ok(())
}

fn resolve_merge_run(
    root: &Path,
    selector: Option<&str>,
    statuses: &[&str],
) -> Result<(PathBuf, MergeRun)> {
    if let Some(selector) = selector {
        let path = resolve_merge_run_selector(root, selector);
        let run: MergeRun = read_json(&path)?;
        if !statuses.is_empty() && !statuses.iter().any(|status| *status == run.status) {
            bail!(
                "Merge run {} is {}, expected one of {}.",
                run.id,
                run.status,
                statuses.join(", ")
            );
        }
        return Ok((path, run));
    }
    latest_merge_run(root, statuses)
}

fn resolve_merge_run_selector(root: &Path, selector: &str) -> PathBuf {
    let path = PathBuf::from(selector);
    if path.exists() {
        return path;
    }
    root.join(".knit/merge-runs")
        .join(format!("{}.json", selector.trim_end_matches(".json")))
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
            step.status.as_str(),
            STEP_SUCCEEDED | STEP_CONFLICTED | STEP_PENDING
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
        .context("No Knit workspace found. Run `knit bundle start \"feature title\"` first.")
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

fn hard_reset(cwd: &Path, reference: &str) {
    let _ = git_output(cwd, ["reset", "--hard", reference]);
}
