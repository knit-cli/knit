use crate::model::RepoEntry;
use crate::store::ActiveBundle;
use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn resolve_repo_indexes(
    active: &ActiveBundle,
    selectors: &[String],
    all: bool,
) -> Result<Vec<usize>> {
    if all && !selectors.is_empty() {
        bail!("Use either --all or repo selectors, not both.");
    }

    if all || selectors.is_empty() {
        return Ok((0..active.bundle.repos.len()).collect());
    }

    let mut indexes = BTreeSet::new();
    for selector in selectors {
        let matches = active
            .bundle
            .repos
            .iter()
            .enumerate()
            .filter_map(|(index, repo)| repo_matches(active, repo, selector).then_some(index))
            .collect::<Vec<_>>();

        if matches.is_empty() {
            bail!("No tracked repo matched `{selector}`.");
        }

        indexes.extend(matches);
    }

    Ok(indexes.into_iter().collect())
}

pub fn repo_matches(active: &ActiveBundle, repo: &RepoEntry, selector: &str) -> bool {
    if selector == repo.id || selector == repo.path {
        return true;
    }

    if repo
        .worktree_path
        .as_ref()
        .is_some_and(|worktree_path| selector == worktree_path)
    {
        return true;
    }

    let selector_path = Path::new(selector);
    if !selector_path.exists() {
        return false;
    }

    let Some(selector_abs) = canonical(selector_path) else {
        return false;
    };

    canonical(Path::new(&repo.path)).is_some_and(|path| path == selector_abs)
        || repo
            .worktree_path
            .as_ref()
            .and_then(|path| canonical(&active.root.join(path)))
            .is_some_and(|path| path == selector_abs)
}

fn canonical(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}
