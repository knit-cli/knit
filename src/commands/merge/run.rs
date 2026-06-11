//! The merge run engine: plan a merge of a source into a branch or bundle
//! target, apply each repo's merge step (with rollback or manual-conflict
//! pause), continue/abort a stopped run, and record the result on a bundle.

use super::report::push_merge_run_steps;
use super::{
    abort_merge_if_needed, acquire_merge_locks, acquire_run_locks, checkout_path_for,
    ensure_checkout_on_branch, ensure_ref_exists, hard_reset, has_unmerged_paths, latest_merge_run,
    load_bundle, merge_in_progress, merge_run_path, resolve_stored_path, MergeRun, MergeRunStatus,
    MergeRunStep, MergeStepStatus, SourcePlan, TargetPlan, MERGE_RUN_KIND,
};
use crate::advice;
use crate::git::{
    branch_exists, current_branch, git_output, is_ancestor, is_git_worktree, ref_exists,
    resolve_base_ref, rev_list, rev_parse,
};
use crate::ids::{node_id, slugify};
use crate::model::{BundleNode, ChangeGroup, RepoChange, RepoEntry, SCHEMA_VERSION};
use crate::output as out;
use crate::store::{
    acquire_named_lock, bundle_exists, bundle_path, load_active_bundle, read_json,
    relative_path_for_storage, write_json,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn start_merge(
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
        status: MergeRunStatus::Running,
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
        if run.steps[index].status != MergeStepStatus::Pending {
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
                run.status = MergeRunStatus::Conflicted;
                run.steps[index].status = MergeStepStatus::Conflicted;
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
                run.steps[index].status = MergeStepStatus::Aborted;
                run.steps[index].message = Some(message.clone());
                rollback_merge_run(root, run)?;
                run.status = MergeRunStatus::Aborted;
                run.updated_at = now_iso();
                write_json(run_path, run)?;
                failures.push(format!("{}: {message}", run.steps[index].repo_id));
                break;
            }
            Err(MergeStepFailure::Fatal(error)) => {
                let message = format!("{error:#}");
                run.steps[index].status = MergeStepStatus::Aborted;
                run.steps[index].message = Some(message.clone());
                rollback_merge_run(root, run)?;
                run.status = MergeRunStatus::Aborted;
                run.updated_at = now_iso();
                write_json(run_path, run)?;
                failures.push(format!("{}: {message}", run.steps[index].repo_id));
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

    if run.steps.iter().all(|step| step.status == MergeStepStatus::Succeeded) {
        finalize_target_bundle(root, run, target_bundle_lock_held)?;
        if run.push_requested {
            if let Err(error) = push_merge_run_steps(root, run, &[], run.set_upstream) {
                run.status = MergeRunStatus::PushFailed;
                run.updated_at = now_iso();
                write_json(run_path, run)?;
                bail!("Merge succeeded locally, but push failed:\n{error:#}");
            }
        }
        run.status = MergeRunStatus::Succeeded;
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

/// How one merge step failed. `Conflict` carries git's conflict report (text
/// is the honest shape there); `Fatal` keeps the full error chain so handlers
/// format it once, at the boundary where it is stored and shown.
enum MergeStepFailure {
    Conflict(String),
    Fatal(anyhow::Error),
}

fn apply_merge_step(
    root: &Path,
    step: &mut MergeRunStep,
    manual: bool,
) -> std::result::Result<(), MergeStepFailure> {
    let checkout = resolve_stored_path(root, &step.checkout_path);
    let status = git_output(&checkout, ["status", "--porcelain"])
        .map_err(MergeStepFailure::Fatal)?;
    if !status.trim().is_empty() {
        return Err(MergeStepFailure::Fatal(anyhow::anyhow!(
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
                rev_parse(&checkout, "HEAD").map_err(MergeStepFailure::Fatal)?,
            );
            step.status = MergeStepStatus::Succeeded;
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
            Err(MergeStepFailure::Fatal(error))
        }
    }
}

pub(super) fn continue_latest_merge(root: &Path) -> Result<()> {
    let (run_path, mut run) = latest_merge_run(root, &[MergeRunStatus::Conflicted])?;
    let _locks = acquire_run_locks(root, &run)?;
    let Some(index) = run
        .steps
        .iter()
        .position(|step| step.status == MergeStepStatus::Conflicted)
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
    run.steps[index].status = MergeStepStatus::Succeeded;
    run.steps[index].message = None;
    println!(
        "{}: {} {} into {}",
        out::repo(&run.steps[index].repo_id),
        out::movement("resolved"),
        out::branch(&run.steps[index].source_ref),
        out::branch(&run.steps[index].target)
    );
    run.status = MergeRunStatus::Running;
    run.updated_at = now_iso();
    write_json(&run_path, &run)?;
    let target_bundle_lock_held = run.target_bundle_id.is_some();
    apply_pending_merge_steps(root, &run_path, &mut run, target_bundle_lock_held)
}

pub(super) fn abort_latest_merge(root: &Path) -> Result<()> {
    let (run_path, mut run) = latest_merge_run(root, &[MergeRunStatus::Conflicted, MergeRunStatus::Running])?;
    let _locks = acquire_run_locks(root, &run)?;
    rollback_merge_run(root, &mut run)?;
    run.status = MergeRunStatus::Aborted;
    run.updated_at = now_iso();
    write_json(&run_path, &run)?;
    println!("{} {}", out::heading("Aborted:"), out::branch(&run.id));
    Ok(())
}

fn rollback_merge_run(root: &Path, run: &mut MergeRun) -> Result<()> {
    for step in run.steps.iter_mut().rev() {
        if step.status == MergeStepStatus::Pending {
            continue;
        }
        let checkout = resolve_stored_path(root, &step.checkout_path);
        abort_merge_if_needed(&checkout);
        hard_reset(&checkout, &step.before_sha);
        step.after_sha = None;
        step.status = MergeStepStatus::Aborted;
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
                    "{}: target bundle checkout is not materialized. Run `knit bundle worktree --bundle {}`.",
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
        target_kind: target.kind(),
        checkout_path: relative_path_for_storage(root, &checkout),
        before_sha,
        after_sha: None,
        status: MergeStepStatus::Pending,
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
                        "{}: source bundle has no feature branch. Run `knit bundle worktree --bundle {}`.",
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
        let commits = rev_list(&checkout, &step.before_sha, after_sha).with_context(|| {
            format!(
                "{}: merged but failed to list commits {}..{} for the bundle ledger",
                step.repo_id, step.before_sha, after_sha
            )
        })?;
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
