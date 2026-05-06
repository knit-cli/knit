use crate::checkout::{checkout_dir, ensure_expected_branch};
use crate::git::git_output;
use crate::model::RepoEntry;
use crate::output as out;
use crate::status::status_label;
use crate::store::{load_active_bundle_for_update, ActiveBundle};
use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn stage_paths(
    explicit_repos: &[String],
    args: &[String],
    intent_to_add: bool,
    update: bool,
) -> Result<()> {
    if intent_to_add && update {
        bail!("Use either --intent-to-add or --update, not both.");
    }
    let active = load_active_bundle_for_update()?;
    let (repos, pathspecs) = resolve_stage_targets(&active, explicit_repos, args)?;

    if intent_to_add && pathspecs.is_empty() {
        bail!("--intent-to-add requires at least one pathspec.");
    }

    for repo in repos {
        let Some(worktree_abs) = checkout_dir(&active, repo) else {
            println!("{}: {}", out::repo(&repo.id), out::muted("no checkout"));
            continue;
        };
        ensure_expected_branch(repo, &worktree_abs)?;
        run_git_add(&worktree_abs, &pathspecs, intent_to_add, update)?;
        let short_status = git_output(&worktree_abs, ["status", "--short"])?;
        println!(
            "{}: {}",
            out::repo(&repo.id),
            out::status(status_label(&short_status))
        );
    }

    Ok(())
}

fn run_git_add(
    checkout: &Path,
    pathspecs: &[String],
    intent_to_add: bool,
    update: bool,
) -> Result<()> {
    let mut args = vec![OsString::from("add")];
    if intent_to_add {
        args.push(OsString::from("-N"));
    } else if update {
        args.push(OsString::from("-u"));
    } else if pathspecs.is_empty() {
        args.push(OsString::from("-A"));
    }

    if !pathspecs.is_empty() {
        args.push(OsString::from("--"));
        args.extend(pathspecs.iter().map(OsString::from));
    }

    git_output(checkout, args)?;
    Ok(())
}

fn resolve_stage_targets<'a>(
    active: &'a ActiveBundle,
    explicit_repos: &[String],
    args: &[String],
) -> Result<(Vec<&'a RepoEntry>, Vec<String>)> {
    if active.bundle.repos.is_empty() {
        bail!("The active bundle has no repos. Run `knit track <repo-path>` first.");
    }

    if !explicit_repos.is_empty() {
        return Ok((resolve_repos(active, explicit_repos)?, args.to_vec()));
    }

    if args.is_empty() {
        return Ok((active.bundle.repos.iter().collect(), Vec::new()));
    }

    if args.iter().all(|arg| selector_exists(active, arg)) {
        return Ok((resolve_repos(active, args)?, Vec::new()));
    }

    if selector_exists(active, &args[0]) {
        return Ok((resolve_repos(active, &args[..1])?, args[1..].to_vec()));
    }

    if active.bundle.repos.len() == 1 {
        return Ok((active.bundle.repos.iter().collect(), args.to_vec()));
    }

    bail!(
        "Pathspecs across multiple repos need a repo selector. Use `knit add <repo> <path>` or `knit add --repo <repo> <path>`."
    );
}

fn resolve_repos<'a>(active: &'a ActiveBundle, selectors: &[String]) -> Result<Vec<&'a RepoEntry>> {
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

    Ok(indexes
        .into_iter()
        .map(|index| &active.bundle.repos[index])
        .collect())
}

fn selector_exists(active: &ActiveBundle, selector: &str) -> bool {
    active
        .bundle
        .repos
        .iter()
        .any(|repo| repo_matches(active, repo, selector))
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
