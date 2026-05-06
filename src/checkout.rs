use crate::git::current_branch;
use crate::model::{RepoEntry, CHECKOUT_MODE_IN_PLACE};
use crate::store::ActiveBundle;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

pub fn is_in_place(repo: &RepoEntry) -> bool {
    repo.checkout_mode == CHECKOUT_MODE_IN_PLACE
}

pub fn checkout_mode_label(repo: &RepoEntry) -> &'static str {
    if is_in_place(repo) {
        "in-place"
    } else {
        "worktree"
    }
}

pub fn checkout_dir(active: &ActiveBundle, repo: &RepoEntry) -> Option<PathBuf> {
    if let Some(path) = &repo.worktree_path {
        let path = resolve_checkout_path(&active.root, path);
        return path.exists().then_some(path);
    }

    if is_in_place(repo) {
        let path = PathBuf::from(&repo.path);
        return path.exists().then_some(path);
    }

    None
}

pub fn checkout_display_path(repo: &RepoEntry) -> String {
    repo.worktree_path
        .clone()
        .unwrap_or_else(|| repo.path.clone())
}

pub fn ensure_expected_branch(repo: &RepoEntry, checkout_dir: &Path) -> Result<()> {
    if !is_in_place(repo) {
        return Ok(());
    }

    let Some(expected) = &repo.feature_branch else {
        bail!(
            "{}: in-place repo has no feature branch recorded. Run `knit worktree` to repair it.",
            repo.id
        );
    };
    let actual = current_branch(checkout_dir)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != *expected {
        bail!(
            "{}: in-place repo is on branch `{actual}`, expected `{expected}`. Checkout the expected branch before running this Knit command.",
            repo.id
        );
    }

    Ok(())
}

pub fn ensure_mutable_checkouts(active: &ActiveBundle) -> Result<()> {
    for repo in &active.bundle.repos {
        if let Some(checkout_dir) = checkout_dir(active, repo) {
            ensure_expected_branch(repo, &checkout_dir)?;
        }
    }

    Ok(())
}

fn resolve_checkout_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}
