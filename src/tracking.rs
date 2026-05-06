use crate::checkout::checkout_dir;
use crate::git::{is_ancestor, merge_base, rev_list, rev_parse};
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

        changes.push(build_repo_change(
            &worktree_dir,
            repo.id.clone(),
            before_sha,
            after_sha,
        )?);
    }

    Ok(changes)
}

pub fn status_note(change: &RepoChange) -> String {
    match change.movement.as_str() {
        "advanced" => format!("unrecorded commits: {}", change.commits.len()),
        "rewound" => format!("rewound commits: {}", change.dropped_commits.len()),
        "diverged" => format!(
            "diverged (+{} -{})",
            change.commits.len(),
            change.dropped_commits.len()
        ),
        _ => "changed".to_string(),
    }
}

pub fn sync_note(change: &RepoChange) -> String {
    match change.movement.as_str() {
        "advanced" => format!("observed {} unrecorded commit(s)", change.commits.len()),
        "rewound" => format!(
            "observed rewind removing {} commit(s)",
            change.dropped_commits.len()
        ),
        "diverged" => format!(
            "observed divergence (+{} -{} commit(s))",
            change.commits.len(),
            change.dropped_commits.len()
        ),
        _ => "observed git movement".to_string(),
    }
}

pub fn sync_observed_changes(active: &mut ActiveBundle) -> Result<Vec<RepoChange>> {
    sync_observed_changes_for_repo_ids(active, None)
}

pub fn sync_observed_changes_for_repo_ids(
    active: &mut ActiveBundle,
    repo_ids: Option<&[String]>,
) -> Result<Vec<RepoChange>> {
    let changes = detect_unrecorded_changes(active)?;
    let changes = match repo_ids {
        Some(repo_ids) => changes
            .into_iter()
            .filter(|change| repo_ids.iter().any(|repo_id| repo_id == &change.repo_id))
            .collect::<Vec<_>>(),
        None => changes,
    };
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
    checkout_dir(active, repo)
}

fn build_repo_change(
    worktree_dir: &PathBuf,
    repo_id: String,
    before_sha: Option<String>,
    after_sha: String,
) -> Result<RepoChange> {
    let Some(before) = before_sha.clone() else {
        return Ok(RepoChange {
            repo_id,
            movement: "advanced".to_string(),
            before_sha,
            after_sha: after_sha.clone(),
            commits: vec![after_sha],
            dropped_commits: Vec::new(),
        });
    };

    if is_ancestor(worktree_dir, &before, &after_sha) {
        return Ok(RepoChange {
            repo_id,
            movement: "advanced".to_string(),
            before_sha,
            after_sha: after_sha.clone(),
            commits: rev_list(worktree_dir, &before, &after_sha)
                .context("failed to list advanced commits")?,
            dropped_commits: Vec::new(),
        });
    }

    if is_ancestor(worktree_dir, &after_sha, &before) {
        return Ok(RepoChange {
            repo_id,
            movement: "rewound".to_string(),
            before_sha,
            after_sha: after_sha.clone(),
            commits: Vec::new(),
            dropped_commits: rev_list(worktree_dir, &after_sha, &before)
                .context("failed to list dropped commits")?,
        });
    }

    let base = merge_base(worktree_dir, &before, &after_sha)?;
    let (commits, dropped_commits) = if let Some(base) = base {
        (
            rev_list(worktree_dir, &base, &after_sha)
                .context("failed to list divergent commits")?,
            rev_list(worktree_dir, &base, &before).context("failed to list replaced commits")?,
        )
    } else {
        (vec![after_sha.clone()], vec![before])
    };

    Ok(RepoChange {
        repo_id,
        movement: "diverged".to_string(),
        before_sha,
        after_sha,
        commits,
        dropped_commits,
    })
}
