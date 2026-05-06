use crate::checkout::checkout_dir;
use crate::git::{current_branch, git_output, rev_parse};
use crate::ids::short_sha;
use crate::model::RepoEntry;
use crate::output as out;
use crate::status::status_label;
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use crate::tracking::{sync_note, sync_observed_changes_for_repo_ids};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn pull_repos(
    selectors: &[String],
    all: bool,
    rebase: bool,
    force: bool,
    feature: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The active bundle has no repos. Run `knit track <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let targets = indexes
        .iter()
        .map(|index| {
            let repo = &active.bundle.repos[*index];
            let cwd = pull_cwd(&active, repo, feature)?;
            Ok(PullTarget {
                repo_index: *index,
                repo_id: repo.id.clone(),
                cwd,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    preflight_clean(&targets, force)?;

    let mut bundle_changed = false;
    for target in &targets {
        let before = rev_parse(&target.cwd, "HEAD")
            .with_context(|| format!("{}: failed to read HEAD before pull", target.repo_id))?;
        run_pull(&target.cwd, rebase)
            .with_context(|| format!("{}: git pull failed", target.repo_id))?;
        let after = rev_parse(&target.cwd, "HEAD")
            .with_context(|| format!("{}: failed to read HEAD after pull", target.repo_id))?;

        print_pull_summary(target, &before, &after);

        if !feature {
            let repo = &mut active.bundle.repos[target.repo_index];
            if repo.base_sha.as_deref() != Some(after.as_str()) {
                repo.base_sha = Some(after);
                bundle_changed = true;
            }
        }
    }

    if feature {
        let repo_ids = targets
            .iter()
            .map(|target| target.repo_id.clone())
            .collect::<Vec<_>>();
        let changes = sync_observed_changes_for_repo_ids(&mut active, Some(&repo_ids))?;
        for change in &changes {
            println!("{}: {}", out::repo(&change.repo_id), sync_note(change));
        }
        bundle_changed = !changes.is_empty();
    }

    if bundle_changed {
        active.bundle.updated_at = now_iso();
        save_active_bundle(&active)?;
    }

    Ok(())
}

struct PullTarget {
    repo_index: usize,
    repo_id: String,
    cwd: PathBuf,
}

fn resolve_repo_indexes(
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

fn pull_cwd(active: &ActiveBundle, repo: &RepoEntry, feature: bool) -> Result<PathBuf> {
    if feature {
        let Some(cwd) = checkout_dir(active, repo) else {
            bail!("{}: no feature checkout is recorded.", repo.id);
        };
        ensure_feature_branch(repo, &cwd)?;
        return Ok(cwd);
    }

    let cwd = PathBuf::from(&repo.path);
    if !cwd.exists() {
        bail!(
            "{}: original repo path does not exist: {}",
            repo.id,
            cwd.display()
        );
    }

    ensure_base_branch(repo, &cwd)?;
    Ok(cwd)
}

fn ensure_feature_branch(repo: &RepoEntry, cwd: &Path) -> Result<()> {
    let expected = repo
        .feature_branch
        .as_deref()
        .with_context(|| format!("{}: no feature branch is recorded.", repo.id))?;
    let actual = current_branch(cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != expected {
        bail!(
            "{}: feature pull expected branch `{expected}`, found `{actual}` in {}.",
            repo.id,
            cwd.display()
        );
    }

    Ok(())
}

fn ensure_base_branch(repo: &RepoEntry, cwd: &Path) -> Result<()> {
    let actual = current_branch(cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual == repo.feature_branch.as_deref().unwrap_or("") {
        bail!(
            "{}: default pull does not operate on the feature branch. Use `knit pull --feature {}` if that is intentional.",
            repo.id,
            repo.id
        );
    }
    if actual != repo.base_branch {
        bail!(
            "{}: default pull expected base branch `{}`, found `{actual}` in {}. Checkout the base branch or use `knit pull --feature {}`.",
            repo.id,
            repo.base_branch,
            cwd.display(),
            repo.id
        );
    }

    Ok(())
}

fn preflight_clean(targets: &[PullTarget], force: bool) -> Result<()> {
    if force {
        return Ok(());
    }

    let mut dirty = Vec::new();
    for target in targets {
        let status = git_output(&target.cwd, ["status", "--short"])
            .with_context(|| format!("{}: failed to inspect status", target.repo_id))?;
        if !status.trim().is_empty() {
            dirty.push(format!(
                "{}: {} in {}",
                target.repo_id,
                status_label(&status),
                target.cwd.display()
            ));
        }
    }

    if !dirty.is_empty() {
        bail!(
            "Refusing to pull with uncommitted changes. Commit, stash, or pass --force to let git decide:\n{}",
            dirty.join("\n")
        );
    }

    Ok(())
}

fn run_pull(cwd: &Path, rebase: bool) -> Result<()> {
    let mut args = vec![OsString::from("pull")];
    if rebase {
        args.push(OsString::from("--rebase"));
    } else {
        args.push(OsString::from("--ff-only"));
    }

    git_output(cwd, args)?;
    Ok(())
}

fn print_pull_summary(target: &PullTarget, before: &str, after: &str) {
    if before == after {
        println!(
            "{}: {} {}",
            out::repo(&target.repo_id),
            out::muted("unchanged"),
            out::sha(short_sha(after))
        );
    } else {
        println!(
            "{}: {} {} -> {}",
            out::repo(&target.repo_id),
            out::movement("advanced"),
            out::sha(short_sha(before)),
            out::sha(short_sha(after))
        );
    }
}

fn repo_matches(active: &ActiveBundle, repo: &RepoEntry, selector: &str) -> bool {
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
