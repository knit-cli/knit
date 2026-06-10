use crate::checkout::{checkout_dir, checkout_display_path};
use crate::git::{git_output, resolve_base_ref};
use crate::ids::short_sha;
use crate::model::RepoEntry;
use crate::output as out;
use crate::store::{load_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn show_diff(selectors: &[String], stat: bool) -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }
    println!(
        "{} {} ({})\n",
        out::heading("Bundle:"),
        out::node(&active.bundle.id),
        active.resolution_source.label()
    );

    let repos = resolve_repos(&active, selectors)?;
    let mut shown = 0usize;

    for repo in repos {
        let Some(checkout) = checkout_dir(&active, repo) else {
            println!(
                "{}: {}",
                out::repo(&repo.id),
                out::danger("checkout missing")
            );
            continue;
        };
        let base = diff_base(repo, &checkout)?;
        let output = run_diff(&checkout, &base, stat)
            .with_context(|| format!("{}: failed to diff against {base}", repo.id))?;

        if output.trim().is_empty() {
            if selectors.is_empty() {
                continue;
            }
            println!("{}: {}", out::repo(&repo.id), out::muted("no diff"));
            continue;
        }

        shown += 1;
        println!(
            "== {} {} {} ==",
            out::repo(&repo.id),
            out::muted("against"),
            out::sha(short_sha(&base))
        );
        println!(
            "{} {}",
            out::muted("checkout:"),
            out::path(checkout_display_path(repo))
        );
        println!("{output}");
    }

    if shown == 0 && selectors.is_empty() {
        println!(
            "{} {}",
            out::ok("No diffs found in bundle"),
            out::node(&active.bundle.id)
        );
    }

    Ok(())
}

fn run_diff(checkout: &Path, base: &str, stat: bool) -> Result<String> {
    if stat {
        git_output(
            checkout,
            [
                OsString::from("diff"),
                OsString::from("--stat"),
                OsString::from(base),
            ],
        )
    } else {
        git_output(checkout, [OsString::from("diff"), OsString::from(base)])
    }
}

fn diff_base(repo: &RepoEntry, checkout: &Path) -> Result<String> {
    if let Some(base_sha) = &repo.base_sha {
        return Ok(base_sha.clone());
    }

    let repo_root = PathBuf::from(&repo.path);
    let base_ref = resolve_base_ref(&repo_root, &repo.base_branch);
    git_output(checkout, ["rev-parse", &base_ref]).map(|sha| sha.trim().to_string())
}

fn resolve_repos<'a>(active: &'a ActiveBundle, selectors: &[String]) -> Result<Vec<&'a RepoEntry>> {
    if selectors.is_empty() {
        return Ok(active.bundle.repos.iter().collect());
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

    Ok(indexes
        .into_iter()
        .map(|index| &active.bundle.repos[index])
        .collect())
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
    crate::paths::canonicalize(path).ok()
}
