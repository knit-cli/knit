//! Resolve what `knit related` was asked about: which project repo a path
//! belongs to (explicit `--repo`, `repo/path` prefix, or cwd inference) and
//! the repo-relative git pathspecs to query.

use crate::ids::slugify;
use crate::model::{KnitProject, ProjectRepoEntry};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

pub(super) struct RelatedTarget {
    pub(super) repo_id: String,
    pub(super) checkout: PathBuf,
    pub(super) paths: Vec<String>,
}

pub(super) fn resolve_related_target(
    root: &Path,
    project: &KnitProject,
    explicit_repo: Option<&str>,
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<RelatedTarget> {
    let repo = match explicit_repo {
        Some(repo_id) => find_project_repo(project, repo_id)
            .with_context(|| format!("No project repo found for `{repo_id}`."))?,
        None => infer_related_repo(root, project, paths, cwd)?,
    };
    let checkout = checkout_for_related_repo(root, repo, cwd);
    let paths = paths
        .iter()
        .map(|path| repo_relative_path(root, repo, &checkout, cwd, path))
        .collect::<Result<Vec<_>>>()?;

    Ok(RelatedTarget {
        repo_id: repo.id.clone(),
        checkout,
        paths,
    })
}

fn find_project_repo<'a>(project: &'a KnitProject, repo_id: &str) -> Option<&'a ProjectRepoEntry> {
    project
        .repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .or_else(|| {
            let slug = slugify(repo_id);
            project.repos.iter().find(|repo| repo.id == slug)
        })
}

fn infer_related_repo<'a>(
    root: &Path,
    project: &'a KnitProject,
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<&'a ProjectRepoEntry> {
    let prefixed = paths
        .iter()
        .filter_map(|path| repo_prefix(project, path).map(|(repo, _)| repo.id.clone()))
        .collect::<BTreeSet<_>>();
    if prefixed.len() == 1 {
        let repo_id = prefixed.iter().next().expect("checked length");
        return find_project_repo(project, repo_id)
            .with_context(|| format!("No project repo found for `{repo_id}`."));
    }
    if prefixed.len() > 1 {
        bail!("Related history queries can inspect one repo at a time. Pass paths for one repo or use --repo.");
    }

    if let Some(repo) = repo_from_cwd(root, project, cwd) {
        return Ok(repo);
    }

    bail!("Could not infer the repo to query. Pass --repo <repo-id> or prefix the path with a project repo id.");
}

fn repo_prefix<'a>(
    project: &'a KnitProject,
    path: &Path,
) -> Option<(&'a ProjectRepoEntry, PathBuf)> {
    if path.is_absolute() {
        return None;
    }
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return None;
    };
    let first = first.to_string_lossy();
    let repo = find_project_repo(project, &first)?;
    let rest = components.collect::<PathBuf>();
    Some((repo, rest))
}

fn repo_from_cwd<'a>(
    root: &Path,
    project: &'a KnitProject,
    cwd: &Path,
) -> Option<&'a ProjectRepoEntry> {
    let cwd = crate::paths::canonicalize(cwd).ok()?;
    for repo in &project.repos {
        let repo_path = absolute_path(root, &repo.path);
        if cwd.starts_with(&repo_path) {
            return Some(repo);
        }
    }

    let worktrees = root.join(".knit/worktrees");
    let relative = cwd.strip_prefix(worktrees).ok()?;
    let mut components = relative.components();
    components.next()?;
    let Some(Component::Normal(repo_id)) = components.next() else {
        return None;
    };
    find_project_repo(project, &repo_id.to_string_lossy())
}

fn checkout_for_related_repo(root: &Path, repo: &ProjectRepoEntry, cwd: &Path) -> PathBuf {
    let source = absolute_path(root, &repo.path);
    let Ok(cwd) = crate::paths::canonicalize(cwd) else {
        return source;
    };
    if cwd.starts_with(&source) {
        return source;
    }

    let worktrees = root.join(".knit/worktrees");
    let Ok(relative) = cwd.strip_prefix(&worktrees) else {
        return source;
    };
    let mut components = relative.components();
    let Some(bundle_id) = components.next() else {
        return source;
    };
    let Some(Component::Normal(repo_id)) = components.next() else {
        return source;
    };
    if repo_id.to_string_lossy() != repo.id {
        return source;
    }

    worktrees.join(bundle_id).join(repo_id)
}

fn repo_relative_path(
    root: &Path,
    repo: &ProjectRepoEntry,
    checkout: &Path,
    cwd: &Path,
    path: &Path,
) -> Result<String> {
    let path = repo_prefix_path(repo, path).unwrap_or_else(|| path.to_path_buf());
    let repo_path = absolute_path(root, &repo.path);

    let relative = if path.is_absolute() {
        strip_path_prefix(&path, checkout)
            .or_else(|| strip_path_prefix(&path, &repo_path))
            .with_context(|| {
                format!(
                    "{} is not inside repo `{}` ({})",
                    path.display(),
                    repo.id,
                    repo_path.display()
                )
            })?
    } else {
        let cwd = crate::paths::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        if cwd.starts_with(checkout) {
            let cwd_relative = cwd.strip_prefix(checkout).unwrap_or(Path::new(""));
            cwd_relative.join(path)
        } else if cwd.starts_with(&repo_path) {
            let cwd_relative = cwd.strip_prefix(&repo_path).unwrap_or(Path::new(""));
            cwd_relative.join(path)
        } else {
            path
        }
    };

    Ok(path_to_git_pathspec(&relative))
}

fn repo_prefix_path(repo: &ProjectRepoEntry, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return None;
    };
    if first.to_string_lossy() != repo.id {
        return None;
    }
    Some(components.collect())
}

fn strip_path_prefix(path: &Path, prefix: &Path) -> Option<PathBuf> {
    let path = crate::paths::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let prefix = crate::paths::canonicalize(prefix).unwrap_or_else(|_| prefix.to_path_buf());
    crate::paths::strip_path_prefix(&path, &prefix)
}

fn path_to_git_pathspec(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        ".".to_string()
    } else {
        text
    }
}

pub(super) fn absolute_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}
