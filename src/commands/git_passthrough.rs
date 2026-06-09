use crate::checkout::checkout_dir;
use crate::model::RepoEntry;
use crate::output as out;
use crate::store::{load_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn run_git(args: &[OsString], explicit_repos: &[String], all: bool) -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let (git_args, selectors) = split_args(&active, args, explicit_repos, all)?;
    if git_args.is_empty() {
        bail!("Pass a git command, for example `knit git status`.");
    }

    let repos = resolve_repos(&active, &selectors)?;
    let multiple = repos.len() > 1;
    let mut failures = Vec::new();

    for repo in repos {
        let cwd = match repo_cwd(&active, repo) {
            Ok(cwd) => cwd,
            Err(error) => {
                failures.push(format!("{}: {error:#}", repo.id));
                continue;
            }
        };
        if multiple {
            println!(
                "== {} ({}) ==",
                out::repo(&repo.id),
                out::path(cwd.display())
            );
        }

        let status = Command::new("git")
            .args(&git_args)
            .current_dir(&cwd)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("failed to run git in {}", cwd.display()))?;

        if !status.success() {
            failures.push(match status.code() {
                Some(code) => format!("{} exited {code}", repo.id),
                None => format!("{} terminated by signal", repo.id),
            });
        }
    }

    if !failures.is_empty() {
        bail!("git command failed: {}", failures.join(", "));
    }

    Ok(())
}

fn split_args(
    active: &ActiveBundle,
    args: &[OsString],
    explicit_repos: &[String],
    all: bool,
) -> Result<(Vec<OsString>, Vec<String>)> {
    if all && !explicit_repos.is_empty() {
        bail!("Use either --all or --repo, not both.");
    }
    if all {
        return Ok((args.to_vec(), vec!["*".to_string()]));
    }
    if !explicit_repos.is_empty() {
        return Ok((args.to_vec(), explicit_repos.to_vec()));
    }

    let mut git_arg_len = args.len();
    let mut selectors = Vec::new();

    while git_arg_len > 0 {
        let Some(value) = args[git_arg_len - 1].to_str() else {
            break;
        };
        if !selector_exists(active, value) {
            break;
        }
        selectors.push(value.to_string());
        git_arg_len -= 1;
    }

    selectors.reverse();
    if selectors.is_empty() {
        selectors.push("*".to_string());
    }

    Ok((args[..git_arg_len].to_vec(), selectors))
}

fn resolve_repos<'a>(active: &'a ActiveBundle, selectors: &[String]) -> Result<Vec<&'a RepoEntry>> {
    let mut indexes = BTreeSet::new();

    for selector in selectors {
        if selector == "*" {
            indexes.extend(0..active.bundle.repos.len());
            continue;
        }

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
    selector == "*"
        || active
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

fn repo_cwd(active: &ActiveBundle, repo: &RepoEntry) -> Result<PathBuf> {
    checkout_dir(active, repo).with_context(|| {
        format!(
            "{} has no active checkout. Run `knit worktree` to materialize it.",
            repo.id
        )
    })
}

fn canonical(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}
