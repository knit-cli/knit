//! Build a compatibility bundle: a fresh bundle whose repos are the union of
//! one or more source bundles' repos.

use crate::ids::{node_id, slugify};
use crate::model::{
    BundleNode, ChangeGroup, RepoEntry, CHECKOUT_MODE_IN_PLACE, CHECKOUT_MODE_WORKTREE,
};
use crate::output as out;
use crate::store::{bundle_path, load_config, save_config, write_json, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub(super) fn create_compat_bundle_from_sources(
    root: &Path,
    title: &str,
    source_bundles: &[ChangeGroup],
    materialize: bool,
    in_place: bool,
    force: bool,
) -> Result<()> {
    let bundle_id = slugify(title);
    let knit_dir = root.join(".knit");
    let bundle_dir = knit_dir.join("bundles");
    let worktree_dir = knit_dir.join("worktrees").join(&bundle_id);
    let path = bundle_path(root, &bundle_id);
    if path.exists() && !force {
        bail!(
            "Bundle {} already exists. Use --force to overwrite it.",
            path.display()
        );
    }

    fs::create_dir_all(&bundle_dir).context("failed to create .knit/bundles")?;
    fs::create_dir_all(&worktree_dir).context("failed to create .knit/worktrees")?;

    let now = now_iso();
    let checkout_mode = if in_place {
        CHECKOUT_MODE_IN_PLACE
    } else {
        CHECKOUT_MODE_WORKTREE
    };
    let mut bundle = ChangeGroup::new(bundle_id.clone(), title.to_string(), now.clone());
    bundle.repos = union_source_repos(source_bundles, checkout_mode)?;
    if !bundle.repos.is_empty() {
        bundle.nodes.push(BundleNode::repos_added(
            node_id("repo"),
            now.clone(),
            bundle.repos.iter().map(|repo| repo.id.clone()).collect(),
        ));
        bundle.head_node_id = bundle.nodes.last().map(|node| node.id.clone());
    }
    write_json(&path, &bundle)?;

    let mut config = load_config(root)?;
    config.active_bundle = Some(bundle_id.clone());
    save_config(root, &config)?;

    if materialize && !bundle.repos.is_empty() {
        let mut active = ActiveBundle::unlocked(root.to_path_buf(), path.clone(), bundle);
        let materialized = crate::commands::worktree::materialize_repos(&mut active, None)?;
        let now = now_iso();
        active.bundle.nodes.push(BundleNode::worktrees_materialized(
            node_id("worktree"),
            now.clone(),
            materialized,
        ));
        active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
        active.bundle.updated_at = now;
        write_json(&path, &active.bundle)?;
    }

    println!(
        "{} {}",
        out::heading("Compatibility bundle:"),
        out::path(path.display())
    );
    Ok(())
}

pub(super) fn union_source_repo_ids(source_bundles: &[ChangeGroup]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut repo_ids = Vec::new();
    for bundle in source_bundles {
        for repo in &bundle.repos {
            if seen.insert(repo.id.clone()) {
                repo_ids.push(repo.id.clone());
            }
        }
    }
    repo_ids
}

fn union_source_repos(source_bundles: &[ChangeGroup], checkout_mode: &str) -> Result<Vec<RepoEntry>> {
    let mut repos: BTreeMap<String, RepoEntry> = BTreeMap::new();
    for bundle in source_bundles {
        for repo in &bundle.repos {
            if let Some(existing) = repos.get(&repo.id) {
                if existing.path != repo.path {
                    bail!(
                        "Repo id {} points at different paths in source bundles: {} and {}.",
                        repo.id,
                        existing.path,
                        repo.path
                    );
                }
                continue;
            }
            repos.insert(
                repo.id.clone(),
                RepoEntry {
                    id: repo.id.clone(),
                    path: repo.path.clone(),
                    remote: repo.remote.clone(),
                    base_branch: repo.base_branch.clone(),
                    checkout_mode: checkout_mode.to_string(),
                    base_sha: repo.base_sha.clone(),
                    feature_branch: None,
                    worktree_path: None,
                    head_sha: None,
                },
            );
        }
    }
    Ok(repos.into_values().collect())
}
