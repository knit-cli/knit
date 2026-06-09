//! Live reshaping of an existing bundle: `knit bundle add/remove/apply-view`.
//!
//! `include` materializes project repos into the bundle (branch + worktree).
//! `exclude` removes repos, tearing down their generated worktree (and optionally
//! the local feature branch). `apply-view` diffs the bundle against a saved view
//! and applies both directions.

use crate::commands::bundle::delete_repo_feature_branch;
use crate::commands::clean::remove_repo_worktree;
use crate::commands::init::{resolve_active_view, resolve_view_repos};
use crate::commands::project::load_project_by_id;
use crate::commands::track::track_project_repos;
use crate::ids::{node_id, slugify};
use crate::model::{BundleNode, ProjectRepoEntry};
use crate::output as out;
use crate::store::{load_active_bundle, load_active_bundle_for_update, save_active_bundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};

pub fn bundle_include(repos: &[String], materialize: bool, in_place: bool) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let project_id = active.bundle.project_id.clone().context(
        "This bundle is not based on a project. Use `knit bundle add <path>` to add a local repo.",
    )?;
    let project = load_project_by_id(&active.root, &project_id)?;

    let mut selected: Vec<ProjectRepoEntry> = Vec::new();
    for repo in repos {
        let id = slugify(repo);
        if active.bundle.repos.iter().any(|tracked| tracked.id == id) {
            bail!("Repo {} is already in this bundle.", out::repo(&id));
        }
        let entry = project
            .repos
            .iter()
            .find(|entry| entry.id == id)
            .with_context(|| {
                format!(
                    "Project {} has no repo named {}.",
                    out::repo(&project_id),
                    out::repo(&id)
                )
            })?;
        selected.push(entry.clone());
    }

    track_project_repos(&mut active, &selected, materialize, in_place)?;
    save_active_bundle(&active)?;
    Ok(())
}

pub fn bundle_exclude(
    repos: &[String],
    keep_worktree: bool,
    delete_branch: bool,
    force: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let root = active.root.clone();
    let bundle_id = active.bundle.id.clone();

    let mut indexes = Vec::new();
    for repo in repos {
        let id = slugify(repo);
        let index = active
            .bundle
            .repos
            .iter()
            .position(|tracked| tracked.id == id)
            .with_context(|| format!("Repo {} is not tracked in this bundle.", out::repo(&id)))?;
        if indexes.contains(&index) {
            bail!("Repo {} was provided more than once.", out::repo(&id));
        }
        indexes.push(index);
    }

    // Highest index first so removals don't shift the ones we still process.
    indexes.sort_unstable_by(|left, right| right.cmp(left));

    let mut removed = Vec::new();
    for index in indexes {
        if !keep_worktree {
            remove_repo_worktree(&root, &bundle_id, &mut active.bundle.repos[index], force)?;
        }
        if delete_branch {
            delete_repo_feature_branch(&active.bundle.repos[index], force)?;
        }
        let repo = active.bundle.repos.remove(index);
        println!(
            "{} repo {} from bundle",
            out::movement("excluded"),
            out::repo(&repo.id)
        );
        removed.push(repo.id);
    }

    let now = now_iso();
    active
        .bundle
        .nodes
        .push(BundleNode::repos_removed(node_id("repo"), now, removed));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;
    Ok(())
}

pub fn bundle_apply_view(
    name: &str,
    keep_worktree: bool,
    delete_branch: bool,
    force: bool,
) -> Result<()> {
    let active = load_active_bundle()?;
    let project_id = active
        .bundle
        .project_id
        .clone()
        .context("This bundle is not based on a project, so views cannot be applied.")?;
    let project = load_project_by_id(&active.root, &project_id)?;
    let view = resolve_active_view(&active.root, &project_id, Some(name))?;
    let target = resolve_view_repos(&project, &[], false, view.as_ref(), &[], &[])?;

    let target_ids: Vec<String> = target.iter().map(|repo| repo.id.clone()).collect();
    let current_ids: Vec<String> = active.bundle.repos.iter().map(|r| r.id.clone()).collect();

    let to_include: Vec<String> = target_ids
        .iter()
        .filter(|id| !current_ids.contains(id))
        .cloned()
        .collect();
    // Only drop repos the project (and thus the view) actually governs; leave
    // any ad-hoc local repos added with `knit bundle add` in place.
    let to_exclude: Vec<String> = current_ids
        .iter()
        .filter(|id| !target_ids.contains(id))
        .filter(|id| project.repos.iter().any(|repo| &repo.id == *id))
        .cloned()
        .collect();

    if to_include.is_empty() && to_exclude.is_empty() {
        println!(
            "{} {}",
            out::muted("Bundle already matches view"),
            out::repo(name)
        );
        return Ok(());
    }

    // load_active_bundle holds no lock; the calls below re-acquire for update.
    drop(active);
    if !to_include.is_empty() {
        bundle_include(&to_include, true, false)?;
    }
    if !to_exclude.is_empty() {
        bundle_exclude(&to_exclude, keep_worktree, delete_branch, force)?;
    }
    println!("{} {}", out::movement("applied view"), out::repo(name));
    Ok(())
}
