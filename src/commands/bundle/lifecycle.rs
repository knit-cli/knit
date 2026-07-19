//! Bundle lifecycle transitions: archive, restore, and delete, including
//! local and `origin` feature-branch deletion for discarded bundles.

use super::{bundle_state, current_root, load_existing_bundle, BundleStatus};
use crate::checkout::is_in_place;
use crate::git::{branch_exists, current_branch, git_output, git_output_optional, ref_exists};
use crate::model::{BundleNode, BundleState, ChangeGroup};
use crate::output as out;
use crate::store::{
    bundle_path as stored_bundle_path, load_config, save_config, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct ArchiveSummary {
    pub(crate) node_id: String,
    pub(crate) removed_worktrees: usize,
}

pub fn archive_bundle(
    bundle_id: &str,
    reason: Option<&str>,
    keep_worktrees: bool,
    force: bool,
) -> Result<()> {
    let reason = archive_reason(reason)?;
    let root = current_root()?;
    let bundle_id = crate::ids::slugify(bundle_id);
    let path = stored_bundle_path(&root, &bundle_id);
    let bundle = load_existing_bundle(&path, &bundle_id)?;
    let mut active = ActiveBundle::unlocked(root.clone(), path.clone(), bundle);
    let summary = archive_active_bundle(&mut active, reason, keep_worktrees, force)?;
    write_json(&path, &active.bundle)?;
    clear_active_if_matches(&root, &bundle_id)?;
    println!(
        "{} {}",
        out::heading("Archived bundle:"),
        out::node(&bundle_id)
    );
    println!("{} {}", out::heading("Node:"), out::node(&summary.node_id));
    println!(
        "{} local feature branches and the bundle artifact",
        out::heading("Preserved:")
    );
    sync_lifecycle_state_to_remote(&active);
    Ok(())
}

/// Mirror an archive/restore onto configured sync remotes by pushing the
/// updated artifact, so hosted dashboards flip lifecycle state together with
/// the local ledger. Best-effort: the local transition already succeeded, so
/// sync failures warn instead of failing the command. A workspace with no
/// push-sync remotes configured is a silent no-op.
fn sync_lifecycle_state_to_remote(active: &ActiveBundle) {
    if let Err(error) =
        crate::commands::remote::sync_active_bundle_to_remote_if_enabled(active, &[], false)
    {
        println!("{} {error:#}", out::warn("remote sync skipped:"));
    }
}

pub(crate) fn archive_active_bundle(
    active: &mut ActiveBundle,
    reason: Option<String>,
    keep_worktrees: bool,
    force: bool,
) -> Result<ArchiveSummary> {
    if bundle_state(&active.bundle) == BundleStatus::Archived {
        bail!("Bundle `{}` is already archived.", active.bundle.id);
    }
    let removed_worktrees = if keep_worktrees {
        0
    } else {
        crate::commands::clean::clean_worktrees_for_bundle(active, force)?
    };
    let now = now_iso();
    let node_id = crate::ids::node_id("archive");
    active.bundle.state = Some(BundleState::Archived);
    active.bundle.archived_at = Some(now.clone());
    active
        .bundle
        .nodes
        .push(BundleNode::feature_archived(node_id.clone(), now, reason));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    Ok(ArchiveSummary {
        node_id,
        removed_worktrees,
    })
}

pub(crate) fn clear_workspace_active_if_matches(root: &Path, bundle_id: &str) -> Result<()> {
    clear_active_if_matches(root, bundle_id)
}

fn archive_reason(reason: Option<&str>) -> Result<Option<String>> {
    match reason {
        Some(reason) => {
            let reason = reason.trim();
            if reason.is_empty() {
                bail!("Archive reason must not be empty when --reason is passed.");
            }
            Ok(Some(reason.to_string()))
        }
        None => Ok(None),
    }
}

pub fn restore_bundle(bundle_id: &str) -> Result<()> {
    let root = current_root()?;
    let bundle_id = crate::ids::slugify(bundle_id);
    let path = stored_bundle_path(&root, &bundle_id);
    let mut bundle = load_existing_bundle(&path, &bundle_id)?;
    if bundle_state(&bundle) != BundleStatus::Archived {
        bail!("Bundle `{bundle_id}` is not archived.");
    }
    bundle.state = Some(BundleState::Open);
    bundle.archived_at = None;
    bundle.updated_at = now_iso();
    write_json(&path, &bundle)?;
    println!(
        "{} {} ({})",
        out::heading("Restored bundle:"),
        out::node(&bundle_id),
        BundleState::Open
    );
    crate::advice::print(
        &root,
        format!("run `knit --bundle {bundle_id} bundle worktree` to rematerialize its checkouts."),
    );
    let active = ActiveBundle::unlocked(root, path, bundle);
    sync_lifecycle_state_to_remote(&active);
    Ok(())
}

pub fn delete_bundle(
    bundle_id: &str,
    force: bool,
    worktrees: bool,
    branches: bool,
    force_branches: bool,
    remote_branches: bool,
    remote_bundles: bool,
    config: Option<&crate::model::KnitConfig>,
) -> Result<()> {
    if !force {
        bail!("Deleting a bundle requires --force.");
    }
    let root = current_root()?;
    let bundle_id = crate::ids::slugify(bundle_id);
    let path = stored_bundle_path(&root, &bundle_id);
    let mut bundle = load_existing_bundle(&path, &bundle_id)?;
    if force_branches && !branches {
        bail!("Use --branches with --force-branches.");
    }
    if remote_branches && !branches {
        bail!("Use --branches with --remote-branches.");
    }
    if branches && !worktrees {
        bail!("Deleting local branches requires --worktrees so generated checkouts are removed first.");
    }
    if remote_bundles {
        let loaded_config;
        let config = match config {
            Some(config) => config,
            None => {
                loaded_config = crate::store::load_effective_config(&root)?;
                &loaded_config
            }
        };
        crate::commands::remote::delete_bundle_from_remote(&root, config, &bundle)?;
    }
    if worktrees {
        let mut active = ActiveBundle::unlocked(root.clone(), path.clone(), bundle);
        crate::commands::clean::clean_worktrees_for_bundle(&mut active, force)?;
        bundle = active.bundle;
    }
    if branches {
        delete_local_feature_branches(&bundle, force_branches)?;
    }
    if remote_branches {
        delete_remote_feature_branches(&bundle)?;
    }
    let now = now_iso();
    bundle.state = Some(BundleState::Deleted);
    bundle.deleted_at = Some(now.clone());
    bundle.updated_at = now;
    let deleted_dir = root.join(".knit/deleted/bundles");
    fs::create_dir_all(&deleted_dir)
        .with_context(|| format!("failed to create {}", deleted_dir.display()))?;
    let deleted_path = deleted_dir.join(format!("{bundle_id}.bundle.json"));
    write_json(&deleted_path, &bundle)?;
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    clear_active_if_matches(&root, &bundle_id)?;
    println!(
        "{} {} {}",
        out::heading("Deleted bundle:"),
        out::node(&bundle_id),
        out::path(deleted_path.display())
    );
    Ok(())
}

/// Delete one repo's local feature branch in its original checkout. Uses
/// `git branch -d` (fails on unmerged/unpushed work) unless `force` selects `-D`.
pub(crate) fn delete_repo_feature_branch(
    repo: &crate::model::RepoEntry,
    force: bool,
) -> Result<()> {
    let Some(branch) = repo.feature_branch.as_deref() else {
        println!(
            "{}: {}",
            out::repo(&repo.id),
            out::muted("no feature branch recorded")
        );
        return Ok(());
    };
    let repo_root = PathBuf::from(&repo.path);
    if !repo_root.exists() {
        bail!(
            "{}: original repo path is missing, cannot delete {}",
            repo.id,
            branch
        );
    }
    if is_in_place(repo) && current_branch(&repo_root)?.as_deref() == Some(branch) {
        bail!(
            "{}: {} is checked out in the source repo; switch branches before deleting it",
            repo.id,
            branch
        );
    }
    if !branch_exists(&repo_root, branch) {
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::muted("branch already missing"),
            out::branch(branch)
        );
        return Ok(());
    }
    let delete_flag = if force { "-D" } else { "-d" };
    git_output(
        &repo_root,
        [
            OsString::from("branch"),
            OsString::from(delete_flag),
            OsString::from(branch),
        ],
    )?;
    println!(
        "{}: {} {}",
        out::repo(&repo.id),
        out::movement("removed"),
        out::branch(branch)
    );
    Ok(())
}

fn delete_local_feature_branches(bundle: &ChangeGroup, force: bool) -> Result<()> {
    let mut failures = Vec::new();
    for repo in &bundle.repos {
        let Some(branch) = repo.feature_branch.as_deref() else {
            println!(
                "{}: {}",
                out::repo(&repo.id),
                out::muted("no feature branch recorded")
            );
            continue;
        };
        let repo_root = PathBuf::from(&repo.path);
        if !repo_root.exists() {
            failures.push(format!(
                "{}: original repo path is missing, cannot delete {}",
                repo.id, branch
            ));
            continue;
        }
        if is_in_place(repo) && current_branch(&repo_root)?.as_deref() == Some(branch) {
            failures.push(format!(
                "{}: {} is checked out in the source repo; switch branches before deleting it",
                repo.id, branch
            ));
            continue;
        }
        if !branch_exists(&repo_root, branch) {
            println!(
                "{}: {} {}",
                out::repo(&repo.id),
                out::muted("branch already missing"),
                out::branch(branch)
            );
            continue;
        }
        let delete_flag = if force { "-D" } else { "-d" };
        let args = vec![
            OsString::from("branch"),
            OsString::from(delete_flag),
            OsString::from(branch),
        ];
        match git_output(&repo_root, args) {
            Ok(_) => println!(
                "{}: {} {}",
                out::repo(&repo.id),
                out::movement("removed"),
                out::branch(branch)
            ),
            Err(error) => failures.push(format!("{}: {error:#}", repo.id)),
        }
    }
    if !failures.is_empty() {
        bail!(
            "failed to delete feature branches:\n{}",
            failures.join("\n")
        );
    }
    Ok(())
}

fn delete_remote_feature_branches(bundle: &ChangeGroup) -> Result<()> {
    let mut failures = Vec::new();
    for repo in &bundle.repos {
        let Some(branch) = repo.feature_branch.as_deref() else {
            println!(
                "{}: {}",
                out::repo(&repo.id),
                out::muted("no feature branch recorded")
            );
            continue;
        };
        let repo_root = PathBuf::from(&repo.path);
        if !repo_root.exists() {
            failures.push(format!(
                "{}: original repo path is missing, cannot delete origin/{}",
                repo.id, branch
            ));
            continue;
        }
        match delete_remote_feature_branch(&repo_root, &repo.id, branch) {
            Ok(()) => {}
            Err(error) => failures.push(format!("{}: {error:#}", repo.id)),
        }
    }
    if !failures.is_empty() {
        bail!(
            "failed to delete remote feature branches:\n{}",
            failures.join("\n")
        );
    }
    Ok(())
}

fn delete_remote_feature_branch(repo_root: &Path, repo_id: &str, branch: &str) -> Result<()> {
    git_output_optional(repo_root, ["remote", "get-url", "origin"])?.with_context(|| {
        format!(
            "{}: no `origin` remote configured in {}",
            repo_id,
            repo_root.display()
        )
    })?;

    let remote = format!("origin/{branch}");
    let remote_heads = git_output(
        repo_root,
        [
            OsString::from("ls-remote"),
            OsString::from("--heads"),
            OsString::from("origin"),
            OsString::from(branch),
        ],
    )?;
    if remote_heads.trim().is_empty() {
        println!(
            "{}: {} {}",
            out::repo(repo_id),
            out::muted("remote branch already missing"),
            out::branch(&remote)
        );
        delete_remote_tracking_ref(repo_root, repo_id, branch)?;
        return Ok(());
    }

    git_output(
        repo_root,
        [
            OsString::from("push"),
            OsString::from("origin"),
            OsString::from("--delete"),
            OsString::from(branch),
        ],
    )?;
    println!(
        "{}: {} {}",
        out::repo(repo_id),
        out::movement("removed"),
        out::branch(&remote)
    );
    delete_remote_tracking_ref(repo_root, repo_id, branch)?;
    Ok(())
}

fn delete_remote_tracking_ref(repo_root: &Path, repo_id: &str, branch: &str) -> Result<()> {
    let remote = format!("origin/{branch}");
    let remote_ref = format!("refs/remotes/{remote}");
    if !ref_exists(repo_root, &remote_ref) {
        return Ok(());
    }
    git_output(
        repo_root,
        [
            OsString::from("branch"),
            OsString::from("-r"),
            OsString::from("-d"),
            OsString::from(&remote),
        ],
    )?;
    println!(
        "{}: {} {}",
        out::repo(repo_id),
        out::movement("removed"),
        out::branch(remote)
    );
    Ok(())
}

pub(super) fn clear_active_if_matches(root: &std::path::Path, bundle_id: &str) -> Result<()> {
    let mut config = load_config(root)?;
    if config.active_bundle.as_deref() == Some(bundle_id) {
        config.active_bundle = None;
        save_config(root, &config)?;
    }
    Ok(())
}
