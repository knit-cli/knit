use crate::checkout::is_in_place;
use crate::git::git_output;
use crate::output as out;
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub fn clean_generated(plans: bool, worktrees: bool, all: bool, force: bool) -> Result<()> {
    if all && (plans || worktrees) {
        bail!("Use either --all or specific clean targets, not both.");
    }
    let clean_plans = all || plans;
    let clean_worktrees = all || worktrees;
    if !clean_plans && !clean_worktrees {
        bail!("Choose what to clean: --plans, --worktrees, or --all.");
    }

    let mut active = load_active_bundle_for_update()?;
    if clean_plans {
        clean_revert_plans(&active)?;
    }
    if clean_worktrees {
        clean_worktrees_for_active_bundle(&mut active, force)?;
        active.bundle.updated_at = now_iso();
        save_active_bundle(&active)?;
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

fn clean_worktrees_for_active_bundle(active: &mut ActiveBundle, force: bool) -> Result<()> {
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

    remove_empty_dir(active.root.join(".knit/worktrees").join(&active.bundle.id));
    Ok(())
}

fn cleanable_worktree_path(
    root: &Path,
    bundle_id: &str,
    repo: &crate::model::RepoEntry,
) -> Option<PathBuf> {
    let recorded = repo.worktree_path.as_deref()?;
    let path = resolve_path(root, recorded);
    let clean_root = root.join(".knit/worktrees").join(bundle_id);
    path.starts_with(clean_root).then_some(path)
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

fn remove_empty_dir(path: PathBuf) {
    let _ = fs::remove_dir(path);
}
