use crate::checkout::{checkout_dir, ensure_expected_branch, ensure_mutable_checkouts};
use crate::git::{git_output, rev_parse};
use crate::ids::{commit_group_id, short_sha};
use crate::model::{BundleNode, CommitGroup, CommitRef, RepoChange};
use crate::output as out;
use crate::status::has_staged_changes;
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use crate::tracking::{sync_note, sync_observed_changes};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;

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

    for target in repos_to_commit {
        git_output(
            &target.worktree_abs,
            [
                OsString::from("commit"),
                OsString::from("-m"),
                OsString::from(&commit_message),
            ],
        )
        .with_context(|| format!("{}: git commit failed", target.repo_id))?;
        let sha = rev_parse(&target.worktree_abs, "HEAD")
            .with_context(|| format!("{}: failed to read commit sha", target.repo_id))?;
        let short = short_sha(&sha);
        println!(
            "{}: {} {}",
            out::repo(&target.repo_id),
            out::movement("committed"),
            out::sha(short)
        );
        commits.push(CommitRef {
            repo_id: target.repo_id.clone(),
            sha: sha.clone(),
        });
        repo_changes.push(RepoChange {
            repo_id: target.repo_id,
            movement: "advanced".to_string(),
            before_sha: Some(target.before_sha),
            after_sha: sha.clone(),
            commits: vec![sha.clone()],
            dropped_commits: Vec::new(),
        });
        active.bundle.repos[target.repo_index].head_sha = Some(sha);
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

pub(crate) fn stage_all_tracked(active: &ActiveBundle) -> Result<()> {
    for repo in &active.bundle.repos {
        let Some(worktree_abs) = checkout_dir(active, repo) else {
            continue;
        };
        ensure_expected_branch(repo, &worktree_abs)?;
        git_output(&worktree_abs, ["add", "-A"])
            .with_context(|| format!("{}: failed to stage changes", repo.id))?;
    }

    Ok(())
}

struct CommitTarget {
    repo_index: usize,
    repo_id: String,
    worktree_abs: PathBuf,
    before_sha: String,
}

fn repos_with_staged_changes(active: &ActiveBundle) -> Result<Vec<CommitTarget>> {
    let mut repos_to_commit = Vec::new();

    for (repo_index, repo) in active.bundle.repos.iter().enumerate() {
        let Some(worktree_abs) = checkout_dir(active, repo) else {
            continue;
        };
        ensure_expected_branch(repo, &worktree_abs)?;
        let short_status = git_output(&worktree_abs, ["status", "--short"])?;
        if has_staged_changes(&short_status) {
            let before_sha = rev_parse(&worktree_abs, "HEAD")
                .with_context(|| format!("{}: failed to read current HEAD", repo.id))?;
            repos_to_commit.push(CommitTarget {
                repo_index,
                repo_id: repo.id.clone(),
                worktree_abs,
                before_sha,
            });
        }
    }

    Ok(repos_to_commit)
}
