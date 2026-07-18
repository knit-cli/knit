//! Per-repo publish execution: push the feature branch (workspace mode),
//! create or adopt the host review object, and fold the result back into the
//! bundle. The `*_from_artifact` variants run without local checkouts.

use super::pr_body::initial_pr_body;
use crate::checkout::checkout_dir;
use crate::git::{current_branch, git_output, git_output_optional, rev_parse};
use crate::ids::short_sha;
use crate::model::{ChangeGroup, RepoEntry};
use crate::output as out;
use crate::providers::{self, pr_number_from_url, publication_for_repo, PrTarget, PullRequest};
use crate::store::ActiveBundle;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::Path;

#[derive(Clone)]
pub(super) struct PublishJob {
    pub(super) repo_index: usize,
    pub(super) repo: RepoEntry,
    pub(super) base_branch: String,
}

pub(super) struct PushedInfo {
    sha: String,
    branch: String,
}

pub(super) enum PublishStatus {
    ExistsRecorded(String),
    FoundExisting(PullRequest),
    Created(PullRequest),
}

pub(super) struct PublishRemoteResult {
    pub(super) repo_index: usize,
    repo_id: String,
    pushed: PushedInfo,
    status: PublishStatus,
}

pub(super) struct ArtifactPublishResult {
    pub(super) repo_index: usize,
    repo_id: String,
    status: PublishStatus,
}

pub(super) fn publish_repo_remote(
    active: &ActiveBundle,
    bundle: &ChangeGroup,
    job: &PublishJob,
    draft: bool,
    renew: bool,
    set_upstream: bool,
) -> Result<PublishRemoteResult> {
    let repo = &job.repo;
    let base_branch = &job.base_branch;
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded. Run `knit bundle worktree`.",
            repo.id
        )
    })?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    ensure_feature_branch(repo, branch, &cwd)?;
    ensure_origin(repo, &cwd)?;
    let forge = providers::for_repo(repo)?;
    let target = PrTarget::checkout(&cwd);

    let sha = rev_parse(&cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature branch HEAD", repo.id))?;
    run_push(&cwd, branch, set_upstream)
        .with_context(|| format!("{}: failed to push {branch}", repo.id))?;
    let pushed = PushedInfo {
        sha,
        branch: format!("origin/{branch}"),
    };

    if let Some(existing) = publication_for_repo(bundle, &repo.id) {
        if existing.base_branch != *base_branch {
            bail!(
                "{}: review object already recorded against {}. Knit records one review object per repo in a bundle; create a new bundle or publish before changing the base.",
                repo.id,
                out::branch(&existing.base_branch)
            );
        }
        if !renew {
            return Ok(PublishRemoteResult {
                repo_index: job.repo_index,
                repo_id: repo.id.clone(),
                pushed,
                status: PublishStatus::ExistsRecorded(existing.url.clone()),
            });
        }
        let summary = forge.view(&target, &existing.url).with_context(|| {
            format!(
                "{}: failed to verify recorded review {} before renewal",
                repo.id, existing.url
            )
        })?;
        ensure_review_can_be_renewed(repo, &summary, Some(&pushed.sha))?;
    }

    if let Some(existing) = forge.find_existing(&target, branch, base_branch)? {
        if renew && review_is_terminal(&existing) {
            ensure_review_has_new_head(repo, &existing, Some(&pushed.sha))?;
        } else {
            return Ok(PublishRemoteResult {
                repo_index: job.repo_index,
                repo_id: repo.id.clone(),
                pushed,
                status: PublishStatus::FoundExisting(existing),
            });
        }
    }

    let title = format!("{} ({})", bundle.title, repo.id);
    let initial_body = initial_pr_body(bundle, &repo.id);
    let url = forge.create(&target, base_branch, branch, &title, &initial_body, draft)?;
    let summary = forge.view(&target, &url).unwrap_or_else(|_| PullRequest {
        number: pr_number_from_url(&url).unwrap_or(0),
        url: url.clone(),
        state: Some("OPEN".to_string()),
        title: Some(title),
        base_ref_name: Some(base_branch.to_string()),
        head_ref_name: Some(branch.to_string()),
        body: None,
        is_draft: None,
        head_ref_oid: None,
        mergeable: None,
        merge_state_status: None,
        review_decision: None,
    });
    Ok(PublishRemoteResult {
        repo_index: job.repo_index,
        repo_id: repo.id.clone(),
        pushed,
        status: PublishStatus::Created(summary),
    })
}

pub(super) fn publish_repo_remote_from_artifact(
    cwd: &Path,
    bundle: &ChangeGroup,
    job: &PublishJob,
    draft: bool,
    renew: bool,
) -> Result<ArtifactPublishResult> {
    let repo = &job.repo;
    let base_branch = &job.base_branch;
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded in the bundle artifact.",
            repo.id
        )
    })?;
    let remote = repo.remote.as_deref().with_context(|| {
        format!(
            "{}: no git remote recorded in the bundle artifact.",
            repo.id
        )
    })?;
    let forge = providers::for_repo(repo)?;
    let repo_full_name = forge
        .repo_full_name(remote)
        .with_context(|| format!("{}: invalid {} remote {remote}", repo.id, forge.id()))?;
    let target = PrTarget::explicit(cwd, repo_full_name);

    if let Some(existing) = publication_for_repo(bundle, &repo.id) {
        if existing.base_branch != *base_branch {
            bail!(
                "{}: review object already recorded against {}. Knit records one review object per repo in a bundle; create a new bundle or publish before changing the base.",
                repo.id,
                out::branch(&existing.base_branch)
            );
        }
        if !renew {
            return Ok(ArtifactPublishResult {
                repo_index: job.repo_index,
                repo_id: repo.id.clone(),
                status: PublishStatus::ExistsRecorded(existing.url.clone()),
            });
        }
        let summary = forge.view(&target, &existing.url).with_context(|| {
            format!(
                "{}: failed to verify recorded review {} before renewal",
                repo.id, existing.url
            )
        })?;
        ensure_review_can_be_renewed(repo, &summary, repo.head_sha.as_deref())?;
    }

    if let Some(existing) = forge.find_existing(&target, branch, base_branch)? {
        if renew && review_is_terminal(&existing) {
            ensure_review_has_new_head(repo, &existing, repo.head_sha.as_deref())?;
        } else {
            return Ok(ArtifactPublishResult {
                repo_index: job.repo_index,
                repo_id: repo.id.clone(),
                status: PublishStatus::FoundExisting(existing),
            });
        }
    }

    let title = format!("{} ({})", bundle.title, repo.id);
    let initial_body = initial_pr_body(bundle, &repo.id);
    let url = forge.create(&target, base_branch, branch, &title, &initial_body, draft)?;
    let summary = forge.view(&target, &url).unwrap_or_else(|_| PullRequest {
        number: pr_number_from_url(&url).unwrap_or(0),
        url: url.clone(),
        state: Some("OPEN".to_string()),
        title: Some(title),
        base_ref_name: Some(base_branch.to_string()),
        head_ref_name: Some(branch.to_string()),
        body: None,
        is_draft: None,
        head_ref_oid: None,
        mergeable: None,
        merge_state_status: None,
        review_decision: None,
    });
    Ok(ArtifactPublishResult {
        repo_index: job.repo_index,
        repo_id: repo.id.clone(),
        status: PublishStatus::Created(summary),
    })
}

pub(super) fn apply_publish_remote_result(
    active: &mut ActiveBundle,
    outcome: &PublishRemoteResult,
) -> Result<bool> {
    println!(
        "{}: {} {} {}",
        out::repo(&outcome.repo_id),
        out::movement("pushed"),
        out::branch(&outcome.pushed.branch),
        out::sha(short_sha(&outcome.pushed.sha))
    );

    let repo = active.bundle.repos[outcome.repo_index].clone();
    let mut changed = false;
    match &outcome.status {
        PublishStatus::ExistsRecorded(url) => {
            println!(
                "{}: {} {}",
                out::repo(&outcome.repo_id),
                out::movement("exists"),
                url
            );
        }
        PublishStatus::FoundExisting(summary) | PublishStatus::Created(summary) => {
            let forge = providers::for_repo(&repo)?;
            providers::upsert_publication(&mut active.bundle, &repo, forge.as_ref(), summary);
            let pr = publication_for_repo(&active.bundle, &outcome.repo_id)
                .expect("publication was just inserted");
            match &outcome.status {
                PublishStatus::FoundExisting(_) => println!(
                    "{}: {} {}",
                    out::repo(&outcome.repo_id),
                    out::movement("exists"),
                    pr.url
                ),
                PublishStatus::Created(_) => println!(
                    "{}: {} #{} {}",
                    out::repo(&outcome.repo_id),
                    out::movement("created"),
                    pr.number,
                    pr.url
                ),
                PublishStatus::ExistsRecorded(_) => unreachable!(),
            }
            changed = true;
        }
    }
    Ok(changed)
}

fn ensure_review_can_be_renewed(
    repo: &RepoEntry,
    review: &PullRequest,
    current_head: Option<&str>,
) -> Result<()> {
    if review_is_open(review) {
        bail!(
            "{}: recorded review {} is still open; --renew only replaces merged or closed reviews.",
            repo.id,
            review.url
        );
    }
    if !review_is_terminal(review) {
        bail!(
            "{}: recorded review {} has unverifiable state `{}`; refusing to renew it.",
            repo.id,
            review.url,
            review.state.as_deref().unwrap_or("unknown")
        );
    }
    ensure_review_has_new_head(repo, review, current_head)
}

fn ensure_review_has_new_head(
    repo: &RepoEntry,
    review: &PullRequest,
    current_head: Option<&str>,
) -> Result<()> {
    if current_head.is_some() && current_head == review.head_ref_oid.as_deref() {
        bail!(
            "{}: review {} already contains the current feature head; add a new commit before --renew.",
            repo.id,
            review.url
        );
    }
    Ok(())
}

fn review_is_open(review: &PullRequest) -> bool {
    review
        .state
        .as_deref()
        .is_some_and(|state| matches!(state.to_ascii_uppercase().as_str(), "OPEN" | "OPENED"))
}

fn review_is_terminal(review: &PullRequest) -> bool {
    review
        .state
        .as_deref()
        .is_some_and(|state| matches!(state.to_ascii_uppercase().as_str(), "MERGED" | "CLOSED"))
}

pub(super) fn apply_artifact_publish_result(
    bundle: &mut ChangeGroup,
    outcome: &ArtifactPublishResult,
) {
    let repo = bundle.repos[outcome.repo_index].clone();
    match &outcome.status {
        PublishStatus::ExistsRecorded(url) => {
            println!(
                "{}: {} {}",
                out::repo(&outcome.repo_id),
                out::movement("exists"),
                url
            );
        }
        PublishStatus::FoundExisting(summary) | PublishStatus::Created(summary) => {
            let forge = providers::for_repo(&repo).expect("forge resolves for published repo");
            providers::upsert_publication(bundle, &repo, forge.as_ref(), summary);
            let pr = publication_for_repo(bundle, &outcome.repo_id)
                .expect("publication was just inserted");
            match &outcome.status {
                PublishStatus::FoundExisting(_) => println!(
                    "{}: {} {}",
                    out::repo(&outcome.repo_id),
                    out::movement("exists"),
                    pr.url
                ),
                PublishStatus::Created(_) => println!(
                    "{}: {} #{} {}",
                    out::repo(&outcome.repo_id),
                    out::movement("created"),
                    pr.number,
                    pr.url
                ),
                PublishStatus::ExistsRecorded(_) => unreachable!(),
            }
        }
    }
}

fn ensure_feature_branch(repo: &RepoEntry, expected: &str, cwd: &Path) -> Result<()> {
    let actual = current_branch(cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != expected {
        bail!(
            "{}: PR publishing expected feature branch `{expected}`, found `{actual}` in {}.",
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
