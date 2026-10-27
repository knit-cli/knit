//! `knit sign`: re-sign a bundle's commits in place, on the same base.
//!
//! The commits keep their trees, messages, authors and order; only the
//! signature header and the resulting SHAs change. Signing is plain git: the
//! rebase runs with `--gpg-sign` and this repository's own configuration
//! (`commit.gpgsign`, `user.signingkey`, `gpg.format`), so it signs exactly
//! when `git rebase --gpg-sign` would and fails with git's own error when no
//! usable key is configured.
//!
//! The rewrite is recorded with [`RewriteKind::Resign`], its own ledger
//! rewrite kind: the observation names every superseded commit and each named
//! commit group maps 1:1 onto its replacement, keeping the same message and
//! author. After the ledger is saved the rewritten branches are pushed with
//! `--force-with-lease` and the bundle artifact and history follow to the sync
//! remotes, so the hosted copy never trails the re-sign; `--no-push` keeps the
//! whole pass local.

use crate::checkout::checkout_dir;
use crate::git::{git_output, git_output_with_env, is_ancestor, rev_parse};
use crate::ids::short_sha;
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::rewrite::{record_rewrite, RepoRewrite, RewriteKind};
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;

/// One repo this command will rewrite.
struct Target {
    repo_id: String,
    path: PathBuf,
    branch: String,
    base: String,
    head: String,
    unsigned: usize,
    total: usize,
}

pub fn sign_bundle_commits(
    selectors: &[String],
    all: bool,
    force: bool,
    no_push: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active
        .root
        .join(".knit/rebase")
        .join(format!("{}.json", active.bundle.id))
        .exists()
    {
        bail!("A bundle rewrite is pending. Run knit rebase --continue or knit rebase --abort before signing.");
    }
    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    if indexes.is_empty() {
        bail!("The resolved bundle has no repos.");
    }

    // Complete preflight across every selected repo before touching any of
    // them: a signing pass that rewrote half a bundle would leave the group
    // mapping split across two bases.
    let mut targets = Vec::new();
    for &index in &indexes {
        if let Some(target) = inspect(&active, index)? {
            targets.push(target);
        }
    }
    let pending: Vec<&Target> = targets
        .iter()
        .filter(|target| force || target.unsigned > 0)
        .collect();
    let pending_ids: std::collections::BTreeSet<&str> = pending
        .iter()
        .map(|target| target.repo_id.as_str())
        .collect();
    for target in &targets {
        if !pending_ids.contains(target.repo_id.as_str()) {
            println!(
                "{}: {} every commit already carries a signature",
                out::repo(&target.repo_id),
                out::muted("(use --force to re-sign them anyway)")
            );
        }
    }
    if pending.is_empty() {
        println!(
            "{}",
            out::ok("Every bundle commit already carries a signature.")
        );
        return Ok(());
    }
    println!(
        "{}",
        out::muted("signing with this repo's git signing config (user.signingkey, gpg.format)")
    );

    let mut rewrites = Vec::new();
    let mut failures = Vec::new();
    for target in &pending {
        match resign(target) {
            Ok(new_head) if new_head == target.head => {
                // Git reported success without moving the branch: the commits
                // are unchanged, so there is nothing to record.
                failures.push(format!(
                    "{}: git rebase left the branch at {} and the commits are still unsigned",
                    target.repo_id,
                    short_sha(&target.head)
                ));
            }
            Ok(new_head) => {
                println!(
                    "{}: {} {} -> {} {}",
                    out::repo(&target.repo_id),
                    out::movement("re-signed"),
                    out::sha(short_sha(&target.head)),
                    out::sha(short_sha(&new_head)),
                    out::muted(format!(
                        "({} of {} commit(s) were unsigned)",
                        target.unsigned, target.total
                    ))
                );
                rewrites.push(RepoRewrite {
                    repo_id: target.repo_id.clone(),
                    old_head: target.head.clone(),
                    new_head,
                    old_base: target.base.clone(),
                    new_base: target.base.clone(),
                });
            }
            Err(error) => failures.push(format!("{}: {error:#}", target.repo_id)),
        }
    }

    // Record what did happen even when a later repo failed: git and the
    // ledger must not disagree about which SHAs the bundle holds.
    let mut pushed_any = false;
    if !rewrites.is_empty() {
        let groups = record_rewrite(&mut active, &rewrites, RewriteKind::Resign, None)?;
        save_active_bundle(&active)?;
        for group in &groups {
            println!("{} {}", out::heading("Commit group:"), out::node(group));
        }
        if no_push {
            println!(
                "{} publish the rewrite with `knit push --force-with-lease`",
                out::heading("Next:")
            );
        } else {
            for target in &pending {
                let rewrite = match rewrites
                    .iter()
                    .find(|rewrite| rewrite.repo_id == target.repo_id)
                {
                    Some(rewrite) => rewrite,
                    None => continue,
                };
                let repo = active
                    .bundle
                    .repos
                    .iter()
                    .find(|repo| repo.id == target.repo_id)
                    .context("rewritten repo vanished from the bundle")?;
                match force_push_leased(&active, repo, &target.branch) {
                    Ok(()) => {
                        pushed_any = true;
                        println!(
                            "{}: {} {} {}",
                            out::repo(&target.repo_id),
                            out::movement("pushed (forced)"),
                            out::branch(format!("origin/{}", target.branch)),
                            out::sha(short_sha(&rewrite.new_head))
                        );
                    }
                    Err(error) => failures.push(format!(
                        "{}: the re-signed branch could not be pushed: {error:#}\nRetry with `knit push --force-with-lease`.",
                        target.repo_id
                    )),
                }
            }
            if pushed_any {
                if let Err(error) = crate::commands::remote::sync_bundle_to_remote_after_push(
                    &mut active,
                    &[],
                    false,
                    crate::commands::push::PushForce::WithLease,
                    true,
                ) {
                    println!(
                        "{} {error:#}",
                        out::warn("bundle artifact/history sync failed:")
                    );
                }
            }
        }
    }
    if !failures.is_empty() {
        bail!("signing failed:\n{}", failures.join("\n"));
    }
    Ok(())
}

/// Force-push one re-signed branch with a lease against the remote tip the
/// bundle recorded. The branch moved by a rewrite, so the lease is what keeps
/// a concurrent push from being silently discarded.
fn force_push_leased(
    active: &ActiveBundle,
    repo: &crate::model::RepoEntry,
    branch: &str,
) -> Result<()> {
    let path = checkout_dir(active, repo).context("missing feature checkout")?;
    let remote = crate::contribution::push_remote(&path, repo)?;
    crate::commands::push::run_push_to(
        &path,
        &remote,
        branch,
        false,
        crate::commands::push::PushForce::WithLease,
    )?;
    crate::contribution::track_source(&path, repo, branch)
}

/// Validate one repo and describe the work. `None` means the repo has no
/// bundle commits at all, so there is nothing to sign.
fn inspect(active: &ActiveBundle, index: usize) -> Result<Option<Target>> {
    let repo = &active.bundle.repos[index];
    let path = checkout_dir(active, repo)
        .with_context(|| format!("{}: checkout is not materialized", repo.id))?;
    let branch = repo
        .feature_branch
        .clone()
        .with_context(|| format!("{}: no feature branch is recorded", repo.id))?;
    let actual = crate::git::current_branch(&path)?.unwrap_or_else(|| "(detached HEAD)".into());
    if actual != branch {
        bail!(
            "{}: signing expected feature branch `{branch}`, found `{actual}` in {}",
            repo.id,
            path.display()
        );
    }
    if !git_output(&path, ["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        bail!(
            "{}: commit or stash the tracked changes in {} before signing",
            repo.id,
            path.display()
        );
    }
    for name in [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
    ] {
        let relative = git_output(&path, ["rev-parse", "--git-path", name])?;
        if path.join(relative).exists() {
            bail!("{}: finish the existing Git operation first", repo.id);
        }
    }
    let base = rev_parse(
        &path,
        repo.base_sha
            .as_deref()
            .with_context(|| format!("{}: no base commit is recorded", repo.id))?,
    )?;
    let head = rev_parse(&path, "HEAD")?;
    if !is_ancestor(&path, &base, &head) {
        bail!(
            "{}: the recorded base {} is not an ancestor of HEAD; run knit sync (and knit rebase) before signing",
            repo.id,
            short_sha(&base)
        );
    }
    // `git rebase` without `--rebase-merges` drops merge commits and replays
    // everything they brought in, so re-signing a branch that merged its base
    // would copy upstream commits onto it under new SHAs.
    let merges = git_output(&path, ["rev-list", "--merges", &format!("{base}..{head}")])?;
    if let Some(merge) = merges.lines().next() {
        bail!(
            "{}: merge commit {} is in the bundle's history; signing rebases the branch, which would flatten the merge and rewrite the commits it brought in. Run knit rebase to replay the work onto the new base first, then sign",
            repo.id,
            short_sha(merge)
        );
    }
    let commits = crate::git::rev_list(&path, &base, &head)?;
    if commits.is_empty() {
        return Ok(None);
    }
    let mut unsigned = 0;
    for sha in &commits {
        if !commit_is_signed(&path, sha)? {
            unsigned += 1;
        }
    }
    Ok(Some(Target {
        repo_id: repo.id.clone(),
        path,
        branch,
        base,
        head,
        unsigned,
        total: commits.len(),
    }))
}

/// Replay `base..HEAD` onto the same base with signatures, using this
/// repository's git signing configuration as-is. `--force-rebase` is what
/// makes git rewrite commits it would otherwise fast-forward past, and
/// `--empty=keep` keeps commits that are or become empty so the result maps
/// 1:1 onto the input.
fn resign(target: &Target) -> Result<String> {
    let args = [
        OsString::from("rebase"),
        OsString::from("--force-rebase"),
        OsString::from("--no-autosquash"),
        OsString::from("--empty=keep"),
        OsString::from("--gpg-sign"),
        OsString::from(&target.base),
    ];
    // GIT_EDITOR keeps a configured sequence editor from opening on a
    // non-interactive rewrite.
    if let Err(error) = git_output_with_env(&target.path, args, &[("GIT_EDITOR", "true")]) {
        // The trees are identical to the originals, so a failure here is a
        // signing problem rather than a conflict — a missing signing key is
        // git's own error to name. Leave no rebase in progress behind either
        // way.
        let _ = git_output(&target.path, ["rebase", "--abort"]);
        return Err(error);
    }
    let head = rev_parse(&target.path, "HEAD")?;
    let actual = crate::git::current_branch(&target.path)?.unwrap_or_default();
    if actual != target.branch {
        bail!("the rebase left {} on `{actual}`", target.path.display());
    }
    for sha in crate::git::rev_list(&target.path, &target.base, &head)? {
        if !commit_is_signed(&target.path, &sha)? {
            bail!(
                "{} is still unsigned after the rebase; check `git config user.signingkey` and `gpg.format`",
                short_sha(&sha)
            );
        }
    }
    Ok(head)
}

/// Whether `sha` carries a signature header. Cheap and offline.
fn commit_is_signed(cwd: &std::path::Path, sha: &str) -> Result<bool> {
    let object = git_output(cwd, ["cat-file", "commit", sha])
        .with_context(|| format!("failed to read commit {sha}"))?;
    let headers = object.split("\n\n").next().unwrap_or_default();
    Ok(headers
        .lines()
        .any(|line| line.starts_with("gpgsig ") || line.starts_with("gpgsig-sha256 ")))
}
