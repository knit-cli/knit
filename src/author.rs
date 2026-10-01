//! Configured author identity and outgoing feature commit policy.
use crate::{
    git::{git_output, git_output_optional, ref_commit_sha, remote_ref_sha},
    model::RepoEntry,
};
use anyhow::{bail, Context, Result};
use std::{ffi::OsString, path::Path, process::Command};

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
/// Explicit author selection also overrides reused messages and amended authors.
pub(crate) fn configure(cwd: &Path, args: &mut Vec<OsString>, command: &mut Command) -> Result<()> {
    let Some(verb) = args.first().and_then(|arg| arg.to_str()) else {
        return Ok(());
    };
    if !matches!(verb, "commit" | "rebase" | "cherry-pick") {
        return Ok(());
    }
    if std::env::var_os("GIT_AUTHOR_NAME").is_some()
        || std::env::var_os("GIT_AUTHOR_EMAIL").is_some()
    {
        eprintln!("warning: ignoring GIT_AUTHOR_NAME/GIT_AUTHOR_EMAIL; Knit uses git-config identity for commits, including rewritten commits");
    }
    command
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL");
    if verb == "commit" {
        let (name, email) = identity(cwd)?;
        command
            .env("GIT_AUTHOR_NAME", &name)
            .env("GIT_AUTHOR_EMAIL", &email);
        // -C, -c and --amend otherwise reuse authors regardless of environment.
        // Git uses the last --author option. Place ours before any pathspec
        // separator, taking care not to interpret message/file values as flags.
        let mut boundary = args.len();
        let mut reset_author = false;
        let mut i = 1;
        while i < args.len() {
            let arg = args[i].to_string_lossy();
            if arg == "--" {
                boundary = i;
                break;
            }
            if arg == "--reset-author" {
                reset_author = true;
            }
            let takes_value = matches!(
                arg.as_ref(),
                "-m" | "--message"
                    | "-F"
                    | "--file"
                    | "-C"
                    | "--reuse-message"
                    | "-c"
                    | "--reedit-message"
                    | "--author"
                    | "--date"
                    | "-t"
                    | "--template"
                    | "--cleanup"
                    | "--trailer"
                    | "--fixup"
                    | "--squash"
                    | "--pathspec-from-file"
            );
            i += if takes_value { 2 } else { 1 };
        }
        if !reset_author {
            args.insert(
                boundary,
                OsString::from(format!("--author={name} <{email}>")),
            );
        }
    }
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

/// Rebase executes this after each selected pick, and retains it in its todo
/// across conflicts. Git's sequencer supplies original-author variables, so
/// reset them explicitly from config inside the exec, not from the parent env.
pub(crate) const REBASE_AUTHOR_EXEC: &str = "GIT_AUTHOR_NAME=\"$(git config --get user.name)\" GIT_AUTHOR_EMAIL=\"$(git config --get user.email)\" git commit --amend --no-edit --allow-empty --reset-author";

pub(crate) fn require_identity(cwd: &Path) -> Result<()> {
    identity(cwd).map(|_| ())
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

/// A dropped pick can still be followed by an exec instruction. Never amend
/// the destination base in that case: it is not one of the selected commits.
pub(crate) fn rebase_author_exec(base: &str) -> String {
    let quoted = base.replace('\'', "'\\''");
    format!(
        "if git merge-base --is-ancestor HEAD '{quoted}'; then :; else {REBASE_AUTHOR_EXEC}; fi"
    )
}
