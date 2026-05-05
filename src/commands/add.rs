use crate::git::{current_branch, git_output_optional, git_root, infer_base_branch};
use crate::ids::{slugify, unique_repo_id};
use crate::model::RepoEntry;
use crate::paths::same_path;
use crate::store::{load_active_bundle, save_active_bundle};
use crate::time::now_iso;
use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::path::Path;

pub fn add_repo(repo_path: &Path, base_override: Option<&str>) -> Result<()> {
    let mut active = load_active_bundle()?;
    let repo_root = git_root(repo_path)?;
    let repo_name = repo_root
        .file_name()
        .and_then(OsStr::to_str)
        .context("repo path has no valid final component")?
        .to_string();
    let repo_path = repo_root.to_string_lossy().to_string();
    let current_branch = current_branch(&repo_root)?;
    let remote = git_output_optional(&repo_root, ["remote", "get-url", "origin"])?;
    let base_branch = match base_override {
        Some(base) => base.to_string(),
        None => infer_base_branch(&repo_root, current_branch.as_deref())?,
    };

    if let Some(index) = active
        .bundle
        .repos
        .iter()
        .position(|repo| same_path(&repo.path, &repo_path))
    {
        let existing = &mut active.bundle.repos[index];
        existing.remote = remote;
        existing.base_branch = base_branch;
        let repo_id = existing.id.clone();
        let path = existing.path.clone();
        active.bundle.updated_at = now_iso();
        save_active_bundle(&active)?;
        println!("Updated repo {} ({})", repo_id, path);
        return Ok(());
    }

    let desired_id = slugify(&repo_name);
    let repo_id = unique_repo_id(&active.bundle, &desired_id);
    active.bundle.repos.push(RepoEntry {
        id: repo_id.clone(),
        path: repo_path,
        remote,
        base_branch,
        feature_branch: None,
        worktree_path: None,
    });
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;

    println!("Added repo {repo_id}");
    Ok(())
}
