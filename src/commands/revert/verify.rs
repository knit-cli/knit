//! Proof that a landed revert undoes the change it targets: per repository,
//! the revert that reached the base is the exact inverse of the original
//! landing's combined diff, and it is on the freshly fetched base branch.
//! Later work on the base, even in the same files, does not affect the proof.

use super::apply::provider_revert_context;
use super::fallback::{
    has_commit, parents, repository_remote, single_parent_commits, DisposableClone,
};
use crate::git::{git_output, is_ancestor};
use crate::model::{BundleNode, ChangeGroup, RepoEntry, RevertVerification};
use crate::output as out;
use crate::providers::{Forge, PrTarget, PullRequest};
use crate::store::ActiveBundle;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The `pr.revert` node whose reviews this landing merged, if it landed one.
pub(crate) fn compensation_landed<'a>(
    bundle: &'a ChangeGroup,
    landed_urls: &[String],
) -> Option<&'a BundleNode> {
    let latest = bundle.nodes.iter().rev().find(|node| {
        matches!(
            node.node_type.as_str(),
            "commit.group"
                | "revert.group"
                | "git.observed"
                | "land.update"
                | "feature.landed"
                | "pr.revert"
        )
    })?;
    (latest.node_type == "pr.revert"
        && !latest.reverted_reviews.is_empty()
        && !landed_urls.is_empty()
        && landed_urls
            .iter()
            .all(|url| latest.publication_urls.contains(url)))
    .then_some(latest)
}

/// Check every landed revert and print the proof. `workspace` resolves
/// credentials and disposable clones exactly as the revert itself did.
pub(crate) fn verify_landed_revert(
    workspace: &Path,
    bundle: &ChangeGroup,
    compensation: &BundleNode,
    landed_urls: &[String],
) -> Result<RevertVerification> {
    let active = ActiveBundle::unlocked(workspace.to_path_buf(), PathBuf::new(), bundle.clone());
    let mut mismatched = BTreeMap::new();
    let mut verified_repos = 0;
    for (repo_id, original_url) in &compensation.reverted_reviews {
        let Some(revert) = bundle.publications.iter().find(|publication| {
            publication.repo_id == *repo_id && landed_urls.contains(&publication.url)
        }) else {
            continue;
        };
        match verify_repo(
            &active,
            compensation.provider.as_deref(),
            repo_id,
            original_url,
            &revert.url,
        ) {
            Ok(Proof::Inverse {
                base,
                original_number,
                files,
            }) => {
                verified_repos += 1;
                println!(
                    "{}: revert verified on {} — exact inverse of PR #{original_number} ({files} file{})",
                    out::repo(repo_id),
                    out::branch(&base),
                    if files == 1 { "" } else { "s" }
                );
            }
            Ok(Proof::Differs {
                base,
                original_number,
                files,
            }) => {
                println!(
                    "{}: {} on {} — not the exact inverse of PR #{original_number} in:",
                    out::repo(repo_id),
                    out::danger("revert does not match"),
                    out::branch(&base),
                );
                for file in &files {
                    println!("  {file}");
                }
                mismatched.insert(repo_id.clone(), files);
            }
            Err(error) => {
                println!(
                    "{}: {} {error:#}",
                    out::repo(repo_id),
                    out::danger("revert not verified:")
                );
                mismatched.insert(repo_id.clone(), vec![format!("(not verified: {error:#})")]);
            }
        }
    }
    let verified = mismatched.is_empty() && verified_repos > 0;
    if verified {
        println!(
            "{} {} is undone in all {verified_repos} repositor{}.",
            out::heading("Revert verified:"),
            bundle.title,
            if verified_repos == 1 { "y" } else { "ies" }
        );
    } else {
        println!(
            "{} {} of {} repositor{} did not verify; the bundle stays open.",
            out::danger("Revert not verified:"),
            mismatched.len(),
            mismatched.len() + verified_repos,
            if mismatched.len() + verified_repos == 1 {
                "y"
            } else {
                "ies"
            }
        );
    }
    Ok(RevertVerification {
        verified,
        mismatched,
    })
}

enum Proof {
    Inverse {
        base: String,
        original_number: u64,
        files: usize,
    },
    Differs {
        base: String,
        original_number: u64,
        files: Vec<String>,
    },
}

fn verify_repo(
    active: &ActiveBundle,
    provider: Option<&str>,
    repo_id: &str,
    original_url: &str,
    revert_url: &str,
) -> Result<Proof> {
    let (index, target, forge) =
        provider_revert_context(active, provider, repo_id, Some(original_url))?;
    let repo = &active.bundle.repos[index];
    let original = forge.view(&target, original_url)?;
    let revert = forge.view(&target, revert_url)?;
    let original_revision = merged_revision(forge.as_ref(), &target, original_url, &original)?;
    let revert_revision = merged_revision(forge.as_ref(), &target, revert_url, &revert)?;
    let base = revert
        .base_ref_name
        .clone()
        .unwrap_or_else(|| repo.base_branch.clone());
    let remote = if crate::contribution::configured(repo) {
        crate::contribution::destination(repo)
            .context("missing target remote")?
            .to_owned()
    } else {
        repository_remote(repo)?
    };

    let source = Path::new(&repo.path);
    let clone;
    let dir = if git_output(source, ["rev-parse", "--git-dir"]).is_ok() {
        source
    } else {
        clone = DisposableClone::of_base(&target, &remote, &base)?;
        clone.path.as_path()
    };
    git_output(dir, ["fetch", "--quiet", "--no-tags", &remote, &base])
        .with_context(|| format!("failed to fetch `{base}` from {remote}"))?;
    let base_tip = git_output(dir, ["rev-parse", "FETCH_HEAD"])?;
    for revision in [&original_revision, &revert_revision] {
        if !has_commit(dir, revision) {
            let _ = git_output(dir, ["fetch", "--quiet", "--no-tags", &remote, revision]);
        }
        if !has_commit(dir, revision) {
            bail!("merged commit {revision} is not available from {remote}");
        }
    }
    if !is_ancestor(dir, &revert_revision, &base_tip) {
        bail!(
            "the merged revert {} of {revert_url} is not on `{base}`",
            crate::ids::short_sha(&revert_revision)
        );
    }

    let original_start = change_start(dir, repo, &original, &original_revision)?;
    // The revert review was cut from a later base, so the bundle's recorded
    // base says nothing about where its commits begin.
    let revert_repo = RepoEntry {
        base_sha: None,
        ..repo.clone()
    };
    let revert_start = change_start(dir, &revert_repo, &revert, &revert_revision)?;

    let mut files = changed_files(dir, &original_start, &original_revision)?;
    files.extend(changed_files(dir, &revert_start, &revert_revision)?);
    let mut differing = Vec::new();
    for file in &files {
        let undone = patch_id(dir, &original_revision, &original_start, file)?;
        let reverted = patch_id(dir, &revert_start, &revert_revision, file)?;
        if undone != reverted {
            differing.push(file.clone());
        }
    }
    Ok(if differing.is_empty() {
        Proof::Inverse {
            base,
            original_number: original.number,
            files: files.len(),
        }
    } else {
        Proof::Differs {
            base,
            original_number: original.number,
            files: differing,
        }
    })
}

fn merged_revision(
    forge: &dyn Forge,
    target: &PrTarget,
    url: &str,
    review: &PullRequest,
) -> Result<String> {
    forge.merged_revision(target, url)?.with_context(|| {
        format!(
            "{} reports no merge commit for PR #{} ({url})",
            forge.id(),
            review.number
        )
    })
}

/// The base commit a landing started from: a merge's first parent, or the
/// parent of the oldest commit a squash or rebase put on the base.
fn change_start(
    dir: &Path,
    repo: &RepoEntry,
    review: &PullRequest,
    revision: &str,
) -> Result<String> {
    let revision_parents = parents(dir, revision)?;
    if revision_parents.len() > 1 {
        return Ok(revision_parents[0].clone());
    }
    let landed = single_parent_commits(dir, repo, review, revision)?;
    let oldest = landed
        .last()
        .context("the landing put no commits on the base")?;
    parents(dir, oldest)?
        .into_iter()
        .next()
        .with_context(|| format!("{oldest} is a root commit"))
}

fn changed_files(dir: &Path, from: &str, to: &str) -> Result<BTreeSet<String>> {
    Ok(
        git_output(dir, ["diff", "--name-only", "--no-renames", from, to])?
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// Git's stable patch identity of one file's change from `from` to `to`;
/// equal for equal changes regardless of line offsets. Without context lines,
/// later edits next to the change cannot make an exact inverse look different.
fn patch_id(dir: &Path, from: &str, to: &str, file: &str) -> Result<String> {
    let diff = git_output(
        dir,
        [
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--no-renames",
            "--unified=0",
            "--binary",
            "--full-index",
            from,
            to,
            "--",
            file,
        ],
    )?;
    let mut child = Command::new("git")
        .args(["patch-id", "--stable"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run git patch-id")?;
    child
        .stdin
        .take()
        .context("git patch-id has no stdin")?
        .write_all(format!("{diff}\n").as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git patch-id failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned())
}
