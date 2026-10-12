//! Compensation for forges without a native revert: the merged review's
//! commits are reverted in a disposable clone of the repository's remote, the
//! branch is pushed, and an ordinary review is opened for it. Nothing here
//! reads or writes the bundle's checkouts, which a landed bundle no longer has.

use crate::git::{git_output, git_output_optional, is_ancestor, merge_base, rev_list};
use crate::model::RepoEntry;
use crate::providers::{Forge, PrTarget, PullRequest};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn create_revert_review(
    forge: &dyn Forge,
    target: &PrTarget,
    repo: &RepoEntry,
    original: &PullRequest,
    selector: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let revision = forge.merged_revision(target, selector)?.with_context(|| {
        format!(
            "{}: {} reports no merge commit for PR #{} ({selector}), so Knit cannot revert it",
            repo.id,
            forge.id(),
            original.number
        )
    })?;
    let base = original
        .base_ref_name
        .clone()
        .unwrap_or_else(|| repo.base_branch.clone());
    let remote = repository_remote(repo)?;
    let clone = DisposableClone::of_base(target, &remote, &base).with_context(|| {
        format!(
            "{}: failed to clone {remote} to revert PR #{}",
            repo.id, original.number
        )
    })?;
    let dir = clone.path.as_path();

    // The clone has no local identity; take the one `knit commit` would use.
    let identity_source = Path::new(&repo.path);
    let (name, email) = crate::author::identity(if identity_source.is_dir() {
        identity_source
    } else {
        dir
    })?;
    git_output(dir, ["config", "user.name", &name])?;
    git_output(dir, ["config", "user.email", &email])?;

    if !has_commit(dir, &revision) {
        let _ = git_output(dir, ["fetch", "--quiet", "--no-tags", "origin", &revision]);
    }
    if !has_commit(dir, &revision) || !is_ancestor(dir, &revision, "HEAD") {
        bail!(
            "{}: merge commit {revision} of PR #{} is not on `{base}` in {remote}; revert the change by hand",
            repo.id,
            original.number
        );
    }

    let branch = format!(
        "knit/revert-{}-{}",
        original.number,
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    );
    git_output(dir, ["checkout", "--quiet", "-b", &branch])?;

    let mut args = vec![OsString::from("revert"), OsString::from("--no-edit")];
    if parents(dir, &revision)?.len() > 1 {
        args.extend([
            OsString::from("-m"),
            OsString::from("1"),
            OsString::from(&revision),
        ]);
    } else {
        args.extend(
            single_parent_commits(dir, repo, original, &revision)?
                .into_iter()
                .map(OsString::from),
        );
    }
    if let Err(error) = git_output(dir, args) {
        let _ = git_output(dir, ["revert", "--abort"]);
        bail!(
            "{}: reverting PR #{} ({selector}) conflicts with the current `{base}`; no revert branch was pushed and no review was opened. Revert it by hand in a checkout of {remote}, push the branch and open the review yourself.\n{error:#}",
            repo.id,
            original.number
        );
    }

    crate::commands::push::push_new_branch(dir, "origin", &branch)
        .with_context(|| format!("{}: failed to push {branch}", repo.id))?;
    forge.create(target, &base, &branch, title, body, false)
}

/// The commits a squash or rebase merge put on the base, newest first.
///
/// A squash merge is one commit that matches none of the review's own
/// commits. A rebase merge copied every review commit onto the base, ending
/// at the merge revision, so each copy is found by patch equivalence.
pub(super) fn single_parent_commits(
    dir: &Path,
    repo: &RepoEntry,
    original: &PullRequest,
    revision: &str,
) -> Result<Vec<String>> {
    let Some(head) = review_head(dir, original) else {
        return Ok(vec![revision.to_owned()]);
    };
    if head == revision {
        // A fast-forward leaves no trace of the old base tip, so only the
        // bundle's recorded base can say where the review began.
        let limit = repo
            .base_sha
            .as_deref()
            .filter(|base| has_commit(dir, base) && is_ancestor(dir, base, &head))
            .with_context(|| {
                format!(
                    "{}: PR #{} was fast-forwarded and no recorded base marks where it began; revert it by hand",
                    repo.id, original.number
                )
            })?;
        let mut commits = rev_list(dir, limit, &head)?;
        commits.reverse();
        return Ok(commits);
    }
    let limit = review_start(dir, repo, &head, revision)?;
    let reviewed = rev_list(dir, &limit, &head)?;
    let landed = git_output(dir, ["cherry", revision, &head, &limit])?
        .lines()
        .filter(|line| line.starts_with('-'))
        .count();
    if landed == 0 {
        return Ok(vec![revision.to_owned()]);
    }
    if landed != reviewed.len() {
        bail!(
            "{}: PR #{} landed only {landed} of its {} commits on the base; revert it by hand",
            repo.id,
            original.number,
            reviewed.len()
        );
    }
    let mut commits = Vec::with_capacity(landed);
    let mut current = revision.to_owned();
    for _ in 0..landed {
        let commit_parents = parents(dir, &current)?;
        let [parent] = commit_parents.as_slice() else {
            bail!(
                "{}: PR #{} was rebased onto the base, but a merge commit sits among its landed commits; revert it by hand",
                repo.id,
                original.number
            );
        };
        commits.push(current.clone());
        current = parent.clone();
    }
    Ok(commits)
}

/// Where the review's own commits begin: the bundle's recorded base when it
/// precedes the review head, otherwise the fork point with the base.
fn review_start(dir: &Path, repo: &RepoEntry, head: &str, revision: &str) -> Result<String> {
    if let Some(base) = repo
        .base_sha
        .as_deref()
        .filter(|base| has_commit(dir, base) && is_ancestor(dir, base, head))
    {
        return Ok(base.to_owned());
    }
    merge_base(dir, head, &format!("{revision}^"))?
        .context("the review head shares no history with the base")
}

fn review_head(dir: &Path, original: &PullRequest) -> Option<String> {
    let head = original.head_ref_oid.clone()?;
    if !has_commit(dir, &head) {
        let mut sources = Vec::new();
        sources.extend(original.head_ref_name.clone());
        sources.push(format!("refs/pull/{}/head", original.number));
        for source in sources {
            let _ = git_output(dir, ["fetch", "--quiet", "--no-tags", "origin", &source]);
            if has_commit(dir, &head) {
                break;
            }
        }
    }
    has_commit(dir, &head).then_some(head)
}

pub(super) fn parents(dir: &Path, commit: &str) -> Result<Vec<String>> {
    Ok(
        git_output(dir, ["rev-list", "--parents", "-n", "1", commit])?
            .split_whitespace()
            .skip(1)
            .map(str::to_owned)
            .collect(),
    )
}

pub(super) fn has_commit(dir: &Path, commit: &str) -> bool {
    git_output_optional(
        dir,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{commit}^{{commit}}"),
        ],
    )
    .ok()
    .flatten()
    .is_some()
}

pub(super) fn repository_remote(repo: &RepoEntry) -> Result<String> {
    if let Some(remote) = repo
        .remote
        .as_deref()
        .filter(|remote| !remote.trim().is_empty())
    {
        return Ok(remote.to_owned());
    }
    let source = Path::new(&repo.path);
    if source.is_dir() {
        if let Some(remote) = git_output_optional(source, ["remote", "get-url", "origin"])? {
            return Ok(remote);
        }
    }
    bail!(
        "{}: no remote is recorded for this repository, so Knit cannot clone it to revert the review",
        repo.id
    )
}

/// A clone inside the workspace, so project-scoped forge credentials resolve
/// for its fetches and pushes exactly as they do for bundle checkouts.
pub(super) struct DisposableClone {
    pub(super) path: PathBuf,
}

impl DisposableClone {
    fn new(target: &PrTarget) -> Result<Self> {
        let parent = crate::store::find_knit_root(&target.cwd)
            .map(|root| root.join(".knit/revert-clones"))
            .unwrap_or_else(std::env::temp_dir);
        fs::create_dir_all(&parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        Ok(Self {
            path: parent.join(format!("knit-{}", crate::ids::revert_group_id())),
        })
    }

    pub(super) fn of_base(target: &PrTarget, remote: &str, base: &str) -> Result<Self> {
        let clone = Self::new(target)?;
        let parent = clone
            .path
            .parent()
            .context("disposable clone path has no parent")?;
        git_output(
            parent,
            [
                OsString::from("clone"),
                OsString::from("--quiet"),
                OsString::from("--no-tags"),
                OsString::from("--branch"),
                OsString::from(base),
                OsString::from(remote),
                clone.path.as_os_str().to_owned(),
            ],
        )?;
        Ok(clone)
    }
}

impl Drop for DisposableClone {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| parent.ends_with(".knit/revert-clones"))
        {
            let _ = fs::remove_dir(parent);
        }
    }
}
