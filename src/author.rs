//! Configured author identity and outgoing feature commit policy.
use crate::{
    git::{git_output, git_output_optional, ref_commit_sha, remote_ref_sha},
    model::RepoEntry,
};
use anyhow::{bail, Context, Result};
use std::{
    ffi::OsString,
    path::Path,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

fn identity(cwd: &Path) -> Result<(String, String)> {
    let read = |key| -> Result<String> {
        let value = git_output_optional(cwd, ["config", "--get", key])?
            .filter(|value| !value.trim().is_empty())
            .with_context(|| {
                format!("Configure {key} in git config before creating or pushing commits")
            })?;
        Ok(value)
    };
    Ok((read("user.name")?, read("user.email")?))
}

/// Ignore ambient author overrides without changing process-global environment.
pub(crate) fn configure(args: &[OsString], command: &mut Command) -> Result<()> {
    if args.first().and_then(|arg| arg.to_str()) != Some("commit") {
        return Ok(());
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    if (std::env::var_os("GIT_AUTHOR_NAME").is_some()
        || std::env::var_os("GIT_AUTHOR_EMAIL").is_some())
        && !WARNED.swap(true, Ordering::Relaxed)
    {
        eprintln!("warning: ignoring GIT_AUTHOR_NAME/GIT_AUTHOR_EMAIL; Knit uses git-config identity for commits it creates");
    }
    command
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL");
    Ok(())
}

fn remote_tip(cwd: &Path, remote: &str, reference: &str) -> Result<Option<String>> {
    let tip = remote_ref_sha(cwd, remote, reference)?;
    if let Some(sha) = &tip {
        if ref_commit_sha(cwd, sha)?.is_none() {
            // Fetch objects only: never replace source tracking refs or lease receipts.
            git_output(
                cwd,
                ["fetch", "--no-tags", "--no-write-fetch-head", remote, sha],
            )?;
        }
    }
    Ok(tip)
}

/// Check commits introduced by this push, using the actual push URL (not its
/// potentially unrelated fetch URL). Exclude the pinned base, the current
/// remote base and commits already reachable from the remote feature tip.
/// Rewritten commits have new IDs and are checked even after an earlier push.
/// The explicit bypass permits foreign authors, but never bypasses signing.
/// This does not change tracking refs, push receipts or force-with-lease policy.
pub fn preflight_push(
    cwd: &Path,
    repo: &RepoEntry,
    push_remote: &str,
    allow_foreign_author: bool,
) -> Result<()> {
    preflight(cwd, repo, push_remote, None, allow_foreign_author, true)
}

/// Check the complete review feature set, including commits already pushed by
/// other tools. Unlike push preflight, the remote feature tip is NOT excluded.
/// `target_override` is the effective target/lane branch on the review's
/// destination remote; None uses the recorded base branch. The pinned bundle
/// base remains excluded in either case. An explicit target must exist.
/// Author bypass never bypasses the configured signing requirement.
pub fn preflight_publish(
    cwd: &Path,
    repo: &RepoEntry,
    push_remote: &str,
    target_override: Option<&str>,
    allow_foreign_author: bool,
) -> Result<()> {
    preflight(
        cwd,
        repo,
        push_remote,
        target_override,
        allow_foreign_author,
        false,
    )
}

fn preflight(
    cwd: &Path,
    repo: &RepoEntry,
    push_remote: &str,
    target_override: Option<&str>,
    allow_foreign_author: bool,
    outgoing_only: bool,
) -> Result<()> {
    let branch = repo
        .feature_branch
        .as_deref()
        .context("missing feature branch")?;
    let feature_ref = format!("refs/heads/{branch}");
    let head = ref_commit_sha(cwd, &feature_ref)?.context("missing local feature branch")?;
    let mut exclusions = Vec::new();
    if let Some(base) = &repo.base_sha {
        exclusions.push(ref_commit_sha(cwd, base)?.context("recorded bundle base is unavailable")?);
    }
    let base_remote = if crate::contribution::configured(repo) {
        crate::contribution::destination(repo)
            .context("missing contribution destination")?
            .to_owned()
    } else {
        crate::contribution::git_remote_url(cwd, push_remote, false)?
    };
    let base_branch = target_override.unwrap_or(&repo.base_branch);
    if base_branch == branch {
        bail!("feature branch must differ from the bundle base for author preflight");
    }
    if let Some(base) = remote_tip(cwd, &base_remote, &format!("refs/heads/{base_branch}"))? {
        exclusions.push(base);
    } else if target_override.is_some() {
        bail!("review target {base_branch} is unavailable for author preflight");
    }
    if exclusions.is_empty() {
        bail!("cannot determine bundle base for author preflight");
    }
    if outgoing_only {
        let push_url = crate::contribution::git_remote_url(cwd, push_remote, true)?;
        if let Some(tip) = remote_tip(cwd, &push_url, &feature_ref)? {
            exclusions.push(tip);
        }
    }
    let mut args = vec!["rev-list".to_owned(), head, "--not".to_owned()];
    args.extend(exclusions);
    args.push("--".into());
    let commits = git_output(cwd, args)?;
    if commits.is_empty() {
        return Ok(());
    }
    let configured = if allow_foreign_author {
        None
    } else {
        Some(identity(cwd)?)
    };
    let signed = git_output_optional(cwd, ["config", "--bool", "--get", "commit.gpgsign"])?
        .as_deref()
        == Some("true");
    let mut failures = Vec::new();
    for sha in commits.lines() {
        let author = crate::git::commit_author(cwd, sha)?;
        let mut reasons = Vec::new();
        if configured
            .as_ref()
            .is_some_and(|(name, email)| author.name != *name || author.email != *email)
        {
            reasons.push(format!(
                "author {} <{}> differs from git-config identity",
                author.name, author.email
            ));
        }
        if signed {
            let object = git_output(cwd, ["cat-file", "commit", sha])?;
            let headers = object.split("\n\n").next().unwrap_or_default();
            if !headers
                .lines()
                .any(|line| line.starts_with("gpgsig ") || line.starts_with("gpgsig-sha256 "))
            {
                reasons.push("unsigned while commit.gpgsign is true".into());
            }
        }
        if !reasons.is_empty() {
            failures.push(format!("{sha}: {}", reasons.join("; ")));
        }
    }
    if !failures.is_empty() {
        let scope = if outgoing_only { "outgoing" } else { "review" };
        bail!("{}: {scope} commit preflight failed:\n{}\nUse --allow-foreign-author to explicitly permit other authors; signing requirements still apply.", repo.id, failures.join("\n"));
    }
    Ok(())
}

/// Only inspect the commits selected by the rewrite range.
pub(crate) fn needs_reauthor(cwd: &Path, base: &str, head: &str) -> Result<bool> {
    let (name, email) = identity(cwd)?;
    for sha in git_output(cwd, ["rev-list", &format!("{base}..{head}")])?.lines() {
        let author = crate::git::commit_author(cwd, sha)?;
        if author.name != name || author.email != email {
            return Ok(true);
        }
    }
    Ok(false)
}
