use crate::commands::worktree::materialize_repos;
use crate::git::{
    current_branch, git_output_optional, git_root, infer_base_branch, resolve_base_ref, rev_parse,
};
use crate::ids::{node_id, slugify, unique_repo_id};
use crate::model::{
    BundleNode, ProjectRepoEntry, RepoEntry, CHECKOUT_MODE_IN_PLACE, CHECKOUT_MODE_WORKTREE,
};
use crate::output as out;
use crate::paths::same_path;
use crate::store::{load_active_bundle_for_update, save_active_bundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub struct RepoPlan {
    desired_id: String,
    path: String,
    remote: Option<String>,
    base_branch: String,
    base_sha: String,
    checkout_mode: String,
}

pub fn track_repos(
    repo_paths: &[PathBuf],
    base_override: Option<&str>,
    materialize: bool,
    in_place: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let checkout_mode = if in_place {
        CHECKOUT_MODE_IN_PLACE
    } else {
        CHECKOUT_MODE_WORKTREE
    };
    let plans = resolve_repo_plans(repo_paths, base_override, checkout_mode)?;
    apply_repo_plans(&mut active, plans, materialize)?;
    save_active_bundle(&active)?;
    Ok(())
}

pub fn track_repo_selectors(
    selectors: &[String],
    base_override: Option<&str>,
    materialize: bool,
    in_place: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let checkout_mode = if in_place {
        CHECKOUT_MODE_IN_PLACE
    } else {
        CHECKOUT_MODE_WORKTREE
    };
    let mut plans = Vec::new();
    for selector in selectors {
        if let Some(plan) = resolve_project_selector(&active, selector, in_place)? {
            plans.push(plan);
        } else {
            plans.push(resolve_repo_plan(
                Path::new(selector),
                base_override,
                None,
                checkout_mode,
            )?);
        }
    }
    ensure_unique_paths(&plans)?;
    apply_repo_plans(&mut active, plans, materialize)?;
    save_active_bundle(&active)?;
    Ok(())
}

pub fn track_project_repos(
    active: &mut crate::store::ActiveBundle,
    repos: &[ProjectRepoEntry],
    materialize: bool,
    in_place: bool,
) -> Result<()> {
    let plans = repos
        .iter()
        .map(|repo| resolve_project_repo_plan(repo, in_place))
        .collect::<Result<Vec<_>>>()?;
    ensure_unique_paths(&plans)?;
    apply_repo_plans(active, plans, materialize)
}

fn apply_repo_plans(
    active: &mut crate::store::ActiveBundle,
    plans: Vec<RepoPlan>,
    materialize: bool,
) -> Result<()> {
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
            existing.checkout_mode = plan.checkout_mode;
            touched_repo_ids.push(existing.id.clone());
            println!(
                "{} {} ({})",
                out::movement("updated"),
                out::repo(&existing.id),
                out::path(&existing.path)
            );
            continue;
        }

        let repo_id = unique_repo_id(&active.bundle, &plan.desired_id);
        active.bundle.repos.push(RepoEntry {
            id: repo_id.clone(),
            path: plan.path,
            remote: plan.remote,
            base_branch: plan.base_branch,
            checkout_mode: plan.checkout_mode,
            base_sha: Some(plan.base_sha),
            feature_branch: None,
            worktree_path: None,
            head_sha: None,
        });
        println!("{} {}", out::movement("added"), out::repo(&repo_id));
        touched_repo_ids.push(repo_id);
    }

    if touched_repo_ids.is_empty() {
        return Ok(());
    }

    let now = now_iso();
    active.bundle.nodes.push(BundleNode::repos_added(
        node_id("repo"),
        now.clone(),
        touched_repo_ids.clone(),
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());

    if materialize {
        let materialized_repo_ids = materialize_repos(active, Some(&touched_repo_ids))?;
        let now = now_iso();
        active.bundle.nodes.push(BundleNode::worktrees_materialized(
            node_id("worktree"),
            now,
            materialized_repo_ids,
        ));
        active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    }

    active.bundle.updated_at = now_iso();
    Ok(())
}

fn resolve_repo_plans(
    repo_paths: &[PathBuf],
    base_override: Option<&str>,
    checkout_mode: &str,
) -> Result<Vec<RepoPlan>> {
    let mut plans: Vec<RepoPlan> = Vec::new();

    for repo_path in repo_paths {
        let plan = resolve_repo_plan(repo_path, base_override, None, checkout_mode)?;
        plans.push(plan);
    }

    ensure_unique_paths(&plans)?;
    Ok(plans)
}

fn resolve_repo_plan(
    repo_path: &Path,
    base_override: Option<&str>,
    desired_id: Option<String>,
    checkout_mode: &str,
) -> Result<RepoPlan> {
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
        desired_id: desired_id.unwrap_or_else(|| slugify(&name)),
        path,
        remote,
        base_branch,
        base_sha,
        checkout_mode: checkout_mode.to_string(),
    })
}

fn resolve_project_selector(
    active: &crate::store::ActiveBundle,
    selector: &str,
    in_place: bool,
) -> Result<Option<RepoPlan>> {
    let Some(project_id) = &active.bundle.project_id else {
        return Ok(None);
    };
    let project_path = active
        .root
        .join(".knit/projects")
        .join(format!("{project_id}.project.json"));
    if !project_path.exists() {
        return Ok(None);
    }
    let project: crate::model::KnitProject = crate::store::read_json(&project_path)?;
    let Some(repo) = project.repos.iter().find(|repo| repo.id == selector) else {
        return Ok(None);
    };
    Ok(Some(resolve_project_repo_plan(repo, in_place)?))
}

fn resolve_project_repo_plan(repo: &ProjectRepoEntry, in_place: bool) -> Result<RepoPlan> {
    let checkout_mode = if in_place {
        CHECKOUT_MODE_IN_PLACE.to_string()
    } else {
        repo.checkout_mode.clone()
    };
    let repo_root = git_root(Path::new(&repo.path))?;
    let base_ref = resolve_base_ref(&repo_root, &repo.base_branch);
    let base_sha = rev_parse(&repo_root, &base_ref)
        .with_context(|| format!("{}: failed to resolve base ref {base_ref}", repo.id))?;
    Ok(RepoPlan {
        desired_id: repo.id.clone(),
        path: repo_root.to_string_lossy().to_string(),
        remote: repo.remote.clone(),
        base_branch: repo.base_branch.clone(),
        base_sha,
        checkout_mode,
    })
}

fn ensure_unique_paths(plans: &[RepoPlan]) -> Result<()> {
    for (index, plan) in plans.iter().enumerate() {
        if plans
            .iter()
            .skip(index + 1)
            .any(|existing| same_path(&existing.path, &plan.path))
        {
            bail!("Repo {} was provided more than once.", plan.path);
        }
    }
    Ok(())
}
