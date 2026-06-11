use crate::checkout::is_in_place;
use crate::git::git_output;
use crate::commands::bundle::BundleStatus;
use crate::model::ChangeGroup;
use crate::output as out;
use crate::store::{
    find_knit_root, load_active_bundle_for_update, read_json, save_active_bundle, write_json,
    ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub fn clean_generated(
    plans: bool,
    worktrees: bool,
    archived: bool,
    merge_worktrees: bool,
    all: bool,
    force: bool,
) -> Result<()> {
    if all && (plans || worktrees || archived || merge_worktrees) {
        bail!("Use either --all or specific clean targets, not both.");
    }
    let clean_plans = all || plans;
    let clean_worktrees = all || worktrees;
    let clean_merge_worktrees = all || merge_worktrees;
    if !clean_plans && !clean_worktrees && !clean_merge_worktrees {
        bail!("Choose what to clean: --plans, --worktrees, --merge-worktrees, or --all.");
    }

    if clean_plans {
        let active = load_active_bundle_for_update()?;
        clean_revert_plans(&active)?;
    }
    if clean_worktrees {
        if archived {
            clean_archived_bundle_worktrees(force)?;
        } else {
            let mut active = load_active_bundle_for_update()?;
            clean_worktrees_for_bundle(&mut active, force)?;
            active.bundle.updated_at = now_iso();
            save_active_bundle(&active)?;
        }
    }
    if clean_merge_worktrees {
        clean_merge_worktrees_for_completed_runs(force)?;
    }

    Ok(())
}

fn clean_revert_plans(active: &ActiveBundle) -> Result<()> {
    let path = active.root.join(".knit/revert-plans");
    if !path.exists() {
        println!("{}", out::muted("No revert plans to clean."));
        return Ok(());
    }

    fs::remove_dir_all(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    println!("{} {}", out::movement("removed"), out::path(path.display()));
    Ok(())
}

pub(crate) fn clean_worktrees_for_bundle(active: &mut ActiveBundle, force: bool) -> Result<()> {
    if active.bundle.repos.is_empty() {
        println!("{}", out::muted("No repos are tracked in this bundle."));
        return Ok(());
    }

    let mut failures = Vec::new();
    for repo in &mut active.bundle.repos {
        if is_in_place(repo) {
            println!(
                "{}: {}",
                out::repo(&repo.id),
                out::muted("in-place checkout preserved")
            );
            continue;
        }

        let Some(path) = cleanable_worktree_path(&active.root, &active.bundle.id, repo) else {
            println!(
                "{}: {}",
                out::repo(&repo.id),
                out::muted("no generated worktree recorded")
            );
            continue;
        };

        if !path.exists() {
            println!(
                "{}: {} {}",
                out::repo(&repo.id),
                out::muted("worktree already missing"),
                out::path(path.display())
            );
            repo.worktree_path = None;
            continue;
        }

        let repo_root = PathBuf::from(&repo.path);
        if !repo_root.exists() {
            failures.push(format!(
                "{}: original repo path is missing, cannot run git worktree remove",
                repo.id
            ));
            continue;
        }

        match remove_git_worktree(&repo_root, &path, force) {
            Ok(()) => {
                println!(
                    "{}: {} {}",
                    out::repo(&repo.id),
                    out::movement("removed"),
                    out::path(path.display())
                );
                repo.worktree_path = None;
            }
            Err(error) => failures.push(format!("{}: {error:#}", repo.id)),
        }
    }

    if !failures.is_empty() {
        bail!("failed to clean worktrees:\n{}", failures.join("\n"));
    }

    remove_empty_dirs(active.root.join(".knit/worktrees").join(&active.bundle.id));
    Ok(())
}

/// Tear down a single repo's generated worktree, clearing its recorded
/// `worktreePath`. `git worktree remove` refuses to remove a dirty checkout
/// unless `force` is set, which is the guard against discarding uncommitted work.
pub(crate) fn remove_repo_worktree(
    root: &Path,
    bundle_id: &str,
    repo: &mut crate::model::RepoEntry,
    force: bool,
) -> Result<()> {
    if is_in_place(repo) {
        println!(
            "{}: {}",
            out::repo(&repo.id),
            out::muted("in-place checkout preserved")
        );
        return Ok(());
    }
    let Some(path) = cleanable_worktree_path(root, bundle_id, repo) else {
        return Ok(());
    };
    if !path.exists() {
        repo.worktree_path = None;
        return Ok(());
    }
    let repo_root = PathBuf::from(&repo.path);
    if !repo_root.exists() {
        bail!(
            "{}: original repo path is missing, cannot run git worktree remove",
            repo.id
        );
    }
    remove_git_worktree(&repo_root, &path, force).with_context(|| {
        format!(
            "{}: failed to remove worktree (pass --force to discard uncommitted work)",
            repo.id
        )
    })?;
    println!(
        "{}: {} {}",
        out::repo(&repo.id),
        out::movement("removed"),
        out::path(path.display())
    );
    repo.worktree_path = None;
    remove_empty_dirs(root.join(".knit/worktrees").join(bundle_id));
    Ok(())
}

fn cleanable_worktree_path(
    root: &Path,
    bundle_id: &str,
    repo: &crate::model::RepoEntry,
) -> Option<PathBuf> {
    let clean_root = root.join(".knit/worktrees").join(bundle_id);
    if let Some(recorded) = repo.worktree_path.as_deref() {
        let path = resolve_path(root, recorded);
        return path.starts_with(&clean_root).then_some(path);
    }
    // The recorded worktree path can be lost while the checkout survives — for
    // example when a bundle is synced back from a remote and localized, which
    // clears the local-only `worktreePath`. Fall back to the conventional
    // location so cleanup still removes the generated worktree (and frees its
    // branch) instead of leaving it to block later branch deletion.
    let conventional = clean_root.join(&repo.id);
    crate::git::is_git_worktree(&conventional).then_some(conventional)
}

fn resolve_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn remove_git_worktree(repo_root: &Path, worktree: &Path, force: bool) -> Result<()> {
    let mut args = vec![OsString::from("worktree"), OsString::from("remove")];
    if force {
        args.push(OsString::from("--force"));
    }
    args.push(worktree.as_os_str().to_os_string());
    git_output(repo_root, args)?;
    Ok(())
}

fn remove_empty_dirs(path: PathBuf) {
    if !path.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(&path) else {
        return;
    };
    for entry in entries.flatten() {
        remove_empty_dirs(entry.path());
    }
    let _ = fs::remove_dir(path);
}

fn clean_archived_bundle_worktrees(force: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        println!("{}", out::muted("No bundles."));
        return Ok(());
    }
    let mut cleaned = 0usize;
    for entry in fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let bundle: ChangeGroup = read_json(&path)?;
        let state = crate::commands::bundle::bundle_state(&bundle);
        if !matches!(state, BundleStatus::Closed | BundleStatus::Archived) {
            continue;
        }
        let mut active = ActiveBundle::unlocked(root.clone(), path.clone(), bundle);
        clean_worktrees_for_bundle(&mut active, force)?;
        active.bundle.updated_at = now_iso();
        write_json(&path, &active.bundle)?;
        cleaned += 1;
    }
    if cleaned == 0 {
        println!("{}", out::muted("No archived bundle worktrees to clean."));
    }
    Ok(())
}

fn clean_merge_worktrees_for_completed_runs(force: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let runs_dir = root.join(".knit/merge-runs");
    if !runs_dir.exists() {
        println!("{}", out::muted("No merge runs to clean."));
        return Ok(());
    }
    let mut removed = 0usize;
    for entry in
        fs::read_dir(&runs_dir).with_context(|| format!("failed to read {}", runs_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let value: serde_json::Value = read_json(&path)?;
        let status = value["status"].as_str().unwrap_or("");
        if !matches!(status, "succeeded" | "aborted") {
            continue;
        }
        let Some(steps) = value["steps"].as_array() else {
            continue;
        };
        for step in steps {
            if step["targetKind"].as_str() != Some("branch") {
                continue;
            }
            let Some(checkout_path) = step["checkoutPath"].as_str() else {
                continue;
            };
            let Some(repo_path) = step["repoPath"].as_str() else {
                continue;
            };
            let checkout = resolve_path(&root, checkout_path);
            if !checkout.exists() {
                continue;
            }
            let status = git_output(&checkout, ["status", "--porcelain"])?;
            if !status.trim().is_empty() && !force {
                println!(
                    "{} {}",
                    out::warn("dirty merge worktree preserved:"),
                    out::path(checkout.display())
                );
                continue;
            }
            remove_git_worktree(std::path::Path::new(repo_path), &checkout, force)?;
            println!(
                "{} {}",
                out::movement("removed"),
                out::path(checkout.display())
            );
            removed += 1;
        }
    }
    remove_empty_dirs(root.join(".knit/merge-worktrees"));
    if removed == 0 {
        println!("{}", out::muted("No clean merge worktrees to remove."));
    }
    Ok(())
}
