use crate::git::git_output;
use crate::ids::{commit_group_id, short_sha};
use crate::model::{CommitGroup, CommitRef};
use crate::status::has_staged_changes;
use crate::store::{load_active_bundle, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;

pub fn commit_staged(message: &str) -> Result<()> {
    let mut active = load_active_bundle()?;
    let repos_to_commit = repos_with_staged_changes(&active)?;

    if repos_to_commit.is_empty() {
        bail!("No staged changes found in bundle worktrees.");
    }

    let group_id = commit_group_id();
    let created_at = now_iso();
    let commit_message = format!(
        "{message}\n\nKnit-Group: {group_id}\nKnit-Bundle: {}",
        active.bundle.id
    );
    let mut commits = Vec::new();

    for (repo_id, worktree_abs) in repos_to_commit {
        git_output(
            &worktree_abs,
            [
                OsString::from("commit"),
                OsString::from("-m"),
                OsString::from(&commit_message),
            ],
        )
        .with_context(|| format!("{repo_id}: git commit failed"))?;
        let sha = git_output(&worktree_abs, ["rev-parse", "HEAD"])
            .with_context(|| format!("{repo_id}: failed to read commit sha"))?;
        let short = short_sha(&sha);
        println!("{repo_id}: committed {short}");
        commits.push(CommitRef {
            repo_id,
            sha: sha.trim().to_string(),
        });
    }

    active.bundle.commit_groups.push(CommitGroup {
        id: group_id.clone(),
        message: message.to_string(),
        created_at,
        commits,
    });
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;

    println!("Recorded commit group {group_id}");
    Ok(())
}

fn repos_with_staged_changes(active: &ActiveBundle) -> Result<Vec<(String, PathBuf)>> {
    let mut repos_to_commit = Vec::new();

    for repo in &active.bundle.repos {
        let Some(worktree_path) = &repo.worktree_path else {
            continue;
        };
        let worktree_abs = active.root.join(worktree_path);
        if !worktree_abs.exists() {
            continue;
        }
        let short_status = git_output(&worktree_abs, ["status", "--short"])?;
        if has_staged_changes(&short_status) {
            repos_to_commit.push((repo.id.clone(), worktree_abs));
        }
    }

    Ok(repos_to_commit)
}
