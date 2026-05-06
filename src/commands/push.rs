use crate::checkout::checkout_dir;
use crate::git::{current_branch, git_output, git_output_optional, rev_parse};
use crate::ids::short_sha;
use crate::model::RepoEntry;
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{load_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::Path;

pub fn push_repos(selectors: &[String], all: bool, set_upstream: bool) -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The active bundle has no repos. Run `knit track <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let mut failures = Vec::new();

    for index in indexes {
        let repo = &active.bundle.repos[index];
        if let Err(error) = push_repo(&active, repo, set_upstream) {
            println!("{}: {}", out::repo(&repo.id), out::danger("push failed"));
            failures.push(format!("{}: {error:#}", repo.id));
        }
    }

    if !failures.is_empty() {
        bail!("push failed:\n{}", failures.join("\n"));
    }

    Ok(())
}

fn push_repo(active: &ActiveBundle, repo: &RepoEntry, set_upstream: bool) -> Result<()> {
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded. Run `knit worktree`.",
            repo.id
        )
    })?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    ensure_feature_branch(repo, branch, &cwd)?;
    ensure_origin(repo, &cwd)?;

    let sha = rev_parse(&cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature branch HEAD", repo.id))?;
    run_push(&cwd, branch, set_upstream)
        .with_context(|| format!("{}: failed to push {branch}", repo.id))?;

    let upstream = if set_upstream {
        read_upstream(&cwd).unwrap_or_else(|| format!("origin/{branch}"))
    } else {
        format!("origin/{branch}")
    };
    println!(
        "{}: {} {} {}",
        out::repo(&repo.id),
        out::movement("pushed"),
        out::branch(upstream),
        out::sha(short_sha(&sha))
    );
    Ok(())
}

fn ensure_feature_branch(repo: &RepoEntry, expected: &str, cwd: &Path) -> Result<()> {
    let actual = current_branch(cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != expected {
        bail!(
            "{}: push expected feature branch `{expected}`, found `{actual}` in {}.",
            repo.id,
            cwd.display()
        );
    }

    Ok(())
}

fn ensure_origin(repo: &RepoEntry, cwd: &Path) -> Result<()> {
    git_output_optional(cwd, ["remote", "get-url", "origin"])?.with_context(|| {
        format!(
            "{}: no `origin` remote configured in {}",
            repo.id,
            cwd.display()
        )
    })?;
    Ok(())
}

fn run_push(cwd: &Path, branch: &str, set_upstream: bool) -> Result<()> {
    let mut args = vec![OsString::from("push")];
    if set_upstream {
        args.push(OsString::from("--set-upstream"));
    }
    args.push(OsString::from("origin"));
    args.push(OsString::from(branch));

    git_output(cwd, args)?;
    Ok(())
}

fn read_upstream(cwd: &Path) -> Option<String> {
    git_output(
        cwd,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
}
