use crate::cli::FetchMode;
use crate::git::{git_output, git_output_optional, ref_commit_sha};
use crate::ids::short_sha;
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::load_active_bundle;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

pub fn fetch_repos(
    selectors: &[String],
    mode: FetchMode,
    remote: Option<&str>,
    no_remote: bool,
) -> Result<()> {
    let fetch_git = matches!(mode, FetchMode::All | FetchMode::Git) && !no_remote;
    let fetch_knit = matches!(mode, FetchMode::All | FetchMode::Knit) && !no_remote;

    let mut git_failures = Vec::new();
    let mut knit_result = Ok(());

    if fetch_git {
        let targets = resolve_fetch_targets(selectors)?;

        let results: Vec<(String, Result<FetchOutcome>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = targets
                .iter()
                .map(|target| {
                    let repo_id = target.repo_id.clone();
                    scope.spawn(move || (repo_id, fetch_repo(target)))
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().expect("fetch worker thread panicked"))
                .collect()
        });

        for (repo_id, result) in results {
            match result {
                Ok(outcome) => {
                    print_fetch_summary(
                        &outcome.repo_id,
                        &outcome.remote_ref,
                        outcome.before.as_deref(),
                        outcome.after.as_deref(),
                    );
                }
                Err(error) => {
                    println!("{}: {}", out::repo(&repo_id), out::danger("fetch failed"));
                    git_failures.push(format!("{repo_id}: {error:#}"));
                }
            }
        }
    }

    if fetch_knit {
        // Bundle fetch is project-wide and needs only the workspace, not a
        // resolvable bundle — it must work from the source root even when
        // several open bundles make the fallback ambiguous.
        let cwd = std::env::current_dir().context("failed to read current directory")?;
        let root = crate::store::find_knit_root(&cwd).context("No Knit workspace found.")?;
        knit_result = crate::commands::fetch_bundles_from_remote(
            &root,
            &crate::store::load_config(&root)?,
            remote,
        );
        if let Err(ref error) = knit_result {
            println!(
                "{}: {}",
                out::muted("bundle fetch"),
                out::danger(error.to_string())
            );
        }
    }

    if !git_failures.is_empty() {
        bail!("fetch failed:\n{}", git_failures.join("\n"));
    }

    // Don't fail entire fetch if knit fetch fails (git succeeded)
    let _ = knit_result;

    Ok(())
}

struct FetchTarget {
    repo_id: String,
    path: String,
    base_branch: String,
}

/// Resolve which repos the git side of `knit fetch` updates. Inside a resolved
/// bundle that is the bundle's repos; when no bundle resolves (a fresh
/// collaborator workspace, before any bundle exists locally) fall back to the
/// active project's repos so fetch still refreshes git refs — and the bundle
/// fetch below still runs — instead of failing outright.
fn resolve_fetch_targets(selectors: &[String]) -> Result<Vec<FetchTarget>> {
    let bundle_error = match load_active_bundle() {
        Ok(active) => {
            if active.bundle.repos.is_empty() {
                bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
            }
            let indexes = resolve_repo_indexes(&active, selectors, false)?;
            return Ok(indexes
                .iter()
                .map(|index| {
                    let repo = &active.bundle.repos[*index];
                    FetchTarget {
                        repo_id: repo.id.clone(),
                        path: repo.path.clone(),
                        base_branch: repo.base_branch.clone(),
                    }
                })
                .collect());
        }
        Err(error) => error,
    };

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let Some(root) = crate::store::find_knit_root(&cwd) else {
        return Err(bundle_error);
    };
    let Some(project_id) = crate::store::load_config(&root)?.active_project else {
        return Err(bundle_error);
    };
    let project = crate::commands::project::load_project_by_id(&root, &project_id)?;
    let targets: Vec<FetchTarget> = project
        .repos
        .iter()
        .filter(|repo| selectors.is_empty() || selectors.iter().any(|s| s == &repo.id))
        .map(|repo| FetchTarget {
            repo_id: repo.id.clone(),
            path: repo.path.clone(),
            base_branch: repo.base_branch.clone(),
        })
        .collect();
    if targets.is_empty() {
        return Err(bundle_error);
    }
    Ok(targets)
}

struct FetchOutcome {
    repo_id: String,
    remote_ref: String,
    before: Option<String>,
    after: Option<String>,
}

fn fetch_repo(repo: &FetchTarget) -> Result<FetchOutcome> {
    let cwd = PathBuf::from(&repo.path);
    if !cwd.exists() {
        bail!("original repo path does not exist: {}", cwd.display());
    }

    let remote = "origin";
    git_output_optional(&cwd, ["remote", "get-url", remote])?
        .with_context(|| format!("no `{remote}` remote configured in {}", cwd.display()))?;

    let remote_ref = format!("{remote}/{}", repo.base_branch);
    let before = ref_commit_sha(&cwd, &remote_ref)?;
    git_output(&cwd, ["fetch", remote])?;
    let after = ref_commit_sha(&cwd, &remote_ref)?;

    Ok(FetchOutcome {
        repo_id: repo.repo_id.clone(),
        remote_ref,
        before,
        after,
    })
}

fn print_fetch_summary(repo_id: &str, remote_ref: &str, before: Option<&str>, after: Option<&str>) {
    match (before, after) {
        (Some(before), Some(after)) if before != after => {
            println!(
                "{}: {} {} {} -> {}",
                out::repo(repo_id),
                out::movement("fetched"),
                out::branch(remote_ref),
                out::sha(short_sha(before)),
                out::sha(short_sha(after))
            );
        }
        (_, Some(after)) => {
            println!(
                "{}: {} {} {}",
                out::repo(repo_id),
                out::muted("fetched"),
                out::branch(remote_ref),
                out::sha(short_sha(after))
            );
        }
        _ => {
            println!(
                "{}: {} {}",
                out::repo(repo_id),
                out::muted("fetched"),
                out::branch(remote_ref)
            );
        }
    }
}
