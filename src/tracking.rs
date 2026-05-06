use crate::git::{rev_list, rev_parse};
use crate::ids::node_id;
use crate::model::{BundleNode, RepoChange, RepoEntry};
use crate::store::ActiveBundle;
use crate::time::now_iso;
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn detect_unrecorded_changes(active: &ActiveBundle) -> Result<Vec<RepoChange>> {
    let mut changes = Vec::new();

    for repo in &active.bundle.repos {
        let Some(worktree_dir) = repo_worktree_dir(active, repo) else {
            continue;
        };
        let after_sha = rev_parse(&worktree_dir, "HEAD")
            .with_context(|| format!("{}: failed to read worktree HEAD", repo.id))?;
        let before_sha = repo.head_sha.clone().or_else(|| repo.base_sha.clone());

        if before_sha.as_deref() == Some(after_sha.as_str()) {
            continue;
        }

        let commits = match &before_sha {
            Some(before) => rev_list(&worktree_dir, before, &after_sha)
                .with_context(|| format!("{}: failed to list unrecorded commits", repo.id))?,
            None => vec![after_sha.clone()],
        };

        changes.push(RepoChange {
            repo_id: repo.id.clone(),
            before_sha,
            after_sha,
            commits,
        });
    }

    Ok(changes)
}

pub fn sync_observed_changes(active: &mut ActiveBundle) -> Result<Vec<RepoChange>> {
    let changes = detect_unrecorded_changes(active)?;
    if changes.is_empty() {
        return Ok(changes);
    }

    for change in &changes {
        if let Some(repo) = active
            .bundle
            .repos
            .iter_mut()
            .find(|repo| repo.id == change.repo_id)
        {
            repo.head_sha = Some(change.after_sha.clone());
        }
    }

    let now = now_iso();
    active.bundle.nodes.push(BundleNode::git_observed(
        node_id("git"),
        now,
        changes.clone(),
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();

    Ok(changes)
}

pub fn repo_worktree_dir(active: &ActiveBundle, repo: &RepoEntry) -> Option<PathBuf> {
    repo.worktree_path
        .as_ref()
        .map(|path| active.root.join(path))
        .filter(|path| path.exists())
}
