use crate::commands::worktree::materialize_repos;
use crate::git::{
    current_branch, git_output_optional, git_root, infer_base_branch, resolve_base_ref, rev_parse,
};
use crate::ids::{node_id, slugify, unique_repo_id};
use crate::model::{BundleNode, RepoEntry};
use crate::paths::same_path;
use crate::store::{load_active_bundle_for_update, save_active_bundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

struct RepoPlan {
    name: String,
    path: String,
    remote: Option<String>,
    base_branch: String,
    base_sha: String,
}

pub fn add_repos(
    repo_paths: &[PathBuf],
    base_override: Option<&str>,
    materialize: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let plans = resolve_repo_plans(repo_paths, base_override)?;
    let mut touched_repo_ids = Vec::new();

    for plan in plans {
        if let Some(index) = active
            .bundle
            .repos
            .iter()
            .position(|repo| same_path(&repo.path, &plan.path))
        {
            let existing = &mut active.bundle.repos[index];
            existing.remote = plan.remote;
            existing.base_branch = plan.base_branch;
            existing.base_sha = Some(plan.base_sha);
            touched_repo_ids.push(existing.id.clone());
            println!("Updated repo {} ({})", existing.id, existing.path);
            continue;
        }

        let desired_id = slugify(&plan.name);
        let repo_id = unique_repo_id(&active.bundle, &desired_id);
        active.bundle.repos.push(RepoEntry {
            id: repo_id.clone(),
            path: plan.path,
            remote: plan.remote,
            base_branch: plan.base_branch,
            base_sha: Some(plan.base_sha),
            feature_branch: None,
            worktree_path: None,
            head_sha: None,
        });
        println!("Added repo {repo_id}");
        touched_repo_ids.push(repo_id);
    }

    let now = now_iso();
    active.bundle.nodes.push(BundleNode::repos_added(
        node_id("repo"),
        now.clone(),
        touched_repo_ids.clone(),
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());

    if materialize {
        let materialized_repo_ids = materialize_repos(&mut active, Some(&touched_repo_ids))?;
        let now = now_iso();
        active.bundle.nodes.push(BundleNode::worktrees_materialized(
            node_id("worktree"),
            now,
            materialized_repo_ids,
        ));
        active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    }

    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;
    Ok(())
}

fn resolve_repo_plans(
    repo_paths: &[PathBuf],
    base_override: Option<&str>,
) -> Result<Vec<RepoPlan>> {
    let mut plans: Vec<RepoPlan> = Vec::new();

    for repo_path in repo_paths {
        let plan = resolve_repo_plan(repo_path, base_override)?;
        if plans
            .iter()
            .any(|existing| same_path(&existing.path, &plan.path))
        {
            bail!("Repo {} was provided more than once.", plan.path);
        }
        plans.push(plan);
    }

    Ok(plans)
}

fn resolve_repo_plan(repo_path: &Path, base_override: Option<&str>) -> Result<RepoPlan> {
    let repo_root = git_root(repo_path)?;
    let name = repo_root
        .file_name()
        .and_then(OsStr::to_str)
        .context("repo path has no valid final component")?
        .to_string();
    let path = repo_root.to_string_lossy().to_string();
    let current_branch = current_branch(&repo_root)?;
    let remote = git_output_optional(&repo_root, ["remote", "get-url", "origin"])?;
    let base_branch = match base_override {
        Some(base) => base.to_string(),
        None => infer_base_branch(&repo_root, current_branch.as_deref())?,
    };
    let base_ref = resolve_base_ref(&repo_root, &base_branch);
    let base_sha = rev_parse(&repo_root, &base_ref)?;

    Ok(RepoPlan {
        name,
        path,
        remote,
        base_branch,
        base_sha,
    })
}
