use crate::checkout::{checkout_dir, ensure_expected_branch, ensure_mutable_checkouts};
use crate::git::{git_output, rev_parse};
use crate::ids::{commit_group_id, short_sha};
use crate::model::{BundleNode, CommitGroup, CommitRef, RepoChange, RepoEntry};
use crate::output as out;
use crate::status::has_staged_changes;
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use crate::tracking::{sync_note, sync_observed_changes};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn commit_staged(message: &str, stage_first: bool) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    ensure_mutable_checkouts(&active)?;
    let observed = sync_observed_changes(&mut active)?;
    for change in &observed {
        println!(
            "{}: {}",
            out::repo(&change.repo_id),
            out::warn(sync_note(change))
        );
    }

    if stage_first {
        stage_all_tracked(&active)?;
    }
    let repos_to_commit = repos_with_staged_changes(&active)?;

    if repos_to_commit.is_empty() {
        if !observed.is_empty() {
            save_active_bundle(&active)?;
        }
        bail!("No staged changes found in tracked checkouts.");
    }

    let group_id = commit_group_id();
    let created_at = now_iso();
    let commit_message = format!(
        "{message}\n\nKnit-Group: {group_id}\nKnit-Bundle: {}",
        active.bundle.id
    );
    let mut commits = Vec::new();
    let mut repo_changes = Vec::new();

    let results: Vec<(String, Result<CommitOutcome>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = repos_to_commit
            .iter()
            .map(|target| {
                let target = target.clone();
                let repo_id = target.repo_id.clone();
                let message = commit_message.clone();
                scope.spawn(move || (repo_id, run_commit(&target, &message)))
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("commit worker thread panicked"))
            .collect()
    });

    let mut failures = Vec::new();
    for (repo_id, result) in results {
        match result {
            Ok(outcome) => {
                println!(
                    "{}: {} {}",
                    out::repo(&repo_id),
                    out::movement("committed"),
                    out::sha(short_sha(&outcome.sha))
                );
                commits.push(CommitRef {
                    repo_id: repo_id.clone(),
                    sha: outcome.sha.clone(),
                });
                repo_changes.push(RepoChange {
                    repo_id,
                    movement: "advanced".to_string(),
                    before_sha: Some(outcome.before_sha),
                    after_sha: outcome.sha.clone(),
                    commits: vec![outcome.sha.clone()],
                    dropped_commits: Vec::new(),
                });
                active.bundle.repos[outcome.repo_index].head_sha = Some(outcome.sha);
            }
            Err(error) => {
                println!("{}: {}", out::repo(&repo_id), out::danger("commit failed"));
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    if !failures.is_empty() {
        bail!("commit failed:\n{}", failures.join("\n"));
    }

    active.bundle.commit_groups.push(CommitGroup {
        id: group_id.clone(),
        message: message.to_string(),
        created_at: created_at.clone(),
        commits: commits.clone(),
    });
    active.bundle.nodes.push(BundleNode::commit_group(
        group_id.clone(),
        created_at,
        message.to_string(),
        commits,
        repo_changes,
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;

    println!(
        "{} {}",
        out::heading("Recorded commit group"),
        out::node(group_id)
    );
    Ok(())
}

struct CommitOutcome {
    repo_index: usize,
    before_sha: String,
    sha: String,
}

fn run_commit(target: &CommitTarget, commit_message: &str) -> Result<CommitOutcome> {
    git_output(
        &target.worktree_abs,
        [
            OsString::from("commit"),
            OsString::from("-m"),
            OsString::from(commit_message),
        ],
    )
    .with_context(|| format!("{}: git commit failed", target.repo_id))?;
    let sha = rev_parse(&target.worktree_abs, "HEAD")
        .with_context(|| format!("{}: failed to read commit sha", target.repo_id))?;
    Ok(CommitOutcome {
        repo_index: target.repo_index,
        before_sha: target.before_sha.clone(),
        sha,
    })
}

pub(crate) fn stage_all_tracked(active: &ActiveBundle) -> Result<()> {
    let targets = worktree_targets(active)?;
    if targets.is_empty() {
        return Ok(());
    }

    let failures: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|target| {
                let repo_id = target.repo_id.clone();
                let worktree_abs = target.worktree_abs.clone();
                scope.spawn(move || {
                    git_output(&worktree_abs, ["add", "-A"])
                        .with_context(|| format!("{repo_id}: failed to stage changes"))
                        .map_err(|error| format!("{error:#}"))
                })
            })
            .collect();

        handles
            .into_iter()
            .filter_map(|handle| handle.join().expect("stage worker thread panicked").err())
            .collect()
    });

    if !failures.is_empty() {
        bail!("stage failed:\n{}", failures.join("\n"));
    }

    Ok(())
}

#[derive(Clone)]
struct CommitTarget {
    repo_index: usize,
    repo_id: String,
    worktree_abs: PathBuf,
    before_sha: String,
}

struct WorktreeTarget {
    repo_id: String,
    worktree_abs: PathBuf,
}

fn worktree_targets(active: &ActiveBundle) -> Result<Vec<WorktreeTarget>> {
    let mut targets = Vec::new();
    for repo in &active.bundle.repos {
        let Some(worktree_abs) = checkout_dir(active, repo) else {
            continue;
        };
        ensure_expected_branch(repo, &worktree_abs)?;
        targets.push(WorktreeTarget {
            repo_id: repo.id.clone(),
            worktree_abs,
        });
    }
    Ok(targets)
}

fn repos_with_staged_changes(active: &ActiveBundle) -> Result<Vec<CommitTarget>> {
    let candidates: Vec<(usize, PathBuf)> = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .filter_map(|(repo_index, repo)| {
            checkout_dir(active, repo).map(|worktree_abs| (repo_index, worktree_abs))
        })
        .collect();

    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let results: Vec<Result<Option<CommitTarget>>> = std::thread::scope(|scope| {
        let active = active;
        let handles: Vec<_> = candidates
            .iter()
            .map(|(repo_index, worktree_abs)| {
                let repo_index = *repo_index;
                let worktree_abs = worktree_abs.clone();
                scope.spawn(move || {
                    let repo = &active.bundle.repos[repo_index];
                    scan_staged_changes(repo_index, repo, &worktree_abs)
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("scan worker thread panicked"))
            .collect()
    });

    let mut repos_to_commit = Vec::new();
    for result in results {
        if let Some(target) = result? {
            repos_to_commit.push(target);
        }
    }
    repos_to_commit.sort_by_key(|target| target.repo_index);
    Ok(repos_to_commit)
}

fn scan_staged_changes(
    repo_index: usize,
    repo: &RepoEntry,
    worktree_abs: &Path,
) -> Result<Option<CommitTarget>> {
    ensure_expected_branch(repo, worktree_abs)?;
    let short_status = git_output(worktree_abs, ["status", "--short"])?;
    if !has_staged_changes(&short_status) {
        return Ok(None);
    }
    let before_sha = rev_parse(worktree_abs, "HEAD")
        .with_context(|| format!("{}: failed to read current HEAD", repo.id))?;
    Ok(Some(CommitTarget {
        repo_index,
        repo_id: repo.id.clone(),
        worktree_abs: worktree_abs.to_path_buf(),
        before_sha,
    }))
}
