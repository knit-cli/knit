use crate::git::{git_output, git_output_optional};
use crate::ids::short_sha;
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::load_active_bundle;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub fn fetch_repos(selectors: &[String], all: bool) -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The active bundle has no repos. Run `knit track <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let mut failures = Vec::new();

    for index in indexes {
        let repo = &active.bundle.repos[index];
        if let Err(error) = fetch_repo(repo) {
            println!("{}: {}", out::repo(&repo.id), out::danger("fetch failed"));
            failures.push(format!("{}: {error:#}", repo.id));
        }
    }

    if !failures.is_empty() {
        bail!("fetch failed:\n{}", failures.join("\n"));
    }

    Ok(())
}

fn fetch_repo(repo: &crate::model::RepoEntry) -> Result<()> {
    let cwd = PathBuf::from(&repo.path);
    if !cwd.exists() {
        bail!("original repo path does not exist: {}", cwd.display());
    }

    let remote = "origin";
    git_output_optional(&cwd, ["remote", "get-url", remote])?
        .with_context(|| format!("no `{remote}` remote configured in {}", cwd.display()))?;

    let remote_ref = format!("{remote}/{}", repo.base_branch);
    let before = ref_sha(&cwd, &remote_ref)?;
    git_output(&cwd, ["fetch", remote])?;
    let after = ref_sha(&cwd, &remote_ref)?;

    print_fetch_summary(&repo.id, &remote_ref, before.as_deref(), after.as_deref());
    Ok(())
}

fn ref_sha(cwd: &Path, reference: &str) -> Result<Option<String>> {
    git_output_optional(
        cwd,
        ["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
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
