//! Shared, conservative pending-work detection.
use crate::git::{git_output, is_git_worktree, ref_exists, resolve_base_ref};
use crate::model::RepoEntry;
use anyhow::{Context, Result};
use std::{fs, path::Path};

/// Uncommitted work found in a checkout, split by whether Git tracks it.
#[derive(Clone, Copy, Default)]
pub(crate) struct Pending {
    pub(crate) tracked: bool,
    pub(crate) untracked: bool,
}

impl Pending {
    pub(crate) fn from_porcelain(status: &str) -> Self {
        let mut pending = Self::default();
        for line in status.lines().filter(|line| !line.trim().is_empty()) {
            if line.starts_with("??") {
                pending.untracked = true;
            } else {
                pending.tracked = true;
            }
        }
        pending
    }
    pub(crate) fn any(self) -> bool {
        self.tracked || self.untracked
    }

    pub(crate) fn merge(&mut self, other: Pending) {
        self.tracked |= other.tracked;
        self.untracked |= other.untracked;
    }
}

/// True when the bundle's feature branch (the local branch or its `origin/`
/// counterpart in the source repo) carries commits the base branch does not.
pub(crate) fn feature_branch_unmerged_commits(repo: &RepoEntry) -> Result<bool> {
    let Some(branch) = repo.feature_branch.as_deref() else {
        return Ok(false);
    };
    let repo_root = Path::new(&repo.path);
    if !repo_root.exists() {
        return Ok(false);
    }
    let base_ref = resolve_base_ref(repo_root, &repo.base_branch);
    for candidate in [branch.to_string(), format!("origin/{branch}")] {
        if !ref_exists(repo_root, &candidate) {
            continue;
        }
        let range = format!("{base_ref}..{candidate}");
        let count = git_output(repo_root, ["rev-list", "--count", &range])
            .with_context(|| format!("failed to count commits in {range}"))?;
        if count.trim() != "0" {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn path_pending_changes(path: &Path) -> Result<Pending> {
    if !path.exists() {
        return Ok(Pending::default());
    }
    if is_git_worktree(path) {
        let status = git_output(path, ["status", "--porcelain"])?;
        return Ok(Pending::from_porcelain(&status));
    }
    // Stray files outside a Git worktree can't be classified, so treat them
    // as tracked changes: they block pruning even with --untracked.
    if path.is_file() {
        return Ok(Pending {
            tracked: true,
            untracked: false,
        });
    }
    let mut pending = Pending::default();
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        pending.merge(path_pending_changes(&entry?.path())?);
        if pending.tracked && pending.untracked {
            break;
        }
    }
    Ok(pending)
}
