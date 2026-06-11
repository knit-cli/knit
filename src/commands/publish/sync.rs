//! PR body cross-link sync: fetch each repo's live review object, record it
//! in the bundle, and upsert the managed Knit block into every PR body. The
//! `*_from_artifact` variants run without local checkouts.

use super::pr_body::{render_knit_pr_block, upsert_knit_pr_block};
use crate::checkout::checkout_dir;
use crate::model::{ChangeGroup, RepoEntry};
use crate::output as out;
use crate::providers::{self, publication_for_repo, PrTarget, PullRequest};
use crate::store::{save_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::path::Path;

enum SyncFetchResult {
    NoReviewObject,
    Summary {
        repo_index: usize,
        summary: PullRequest,
    },
}

enum SyncBodyResult {
    Synced(String),
    AlreadySynced,
}

fn fetch_pr_summary_for_sync(
    active: &ActiveBundle,
    repo_index: usize,
    repo: &RepoEntry,
) -> Result<SyncFetchResult> {
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded. Run `knit bundle worktree`.",
            repo.id
        )
    })?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    let forge = providers::for_repo(repo)?;
    let target = PrTarget::checkout(&cwd);

    let summary = if let Some(pr) = publication_for_repo(&active.bundle, &repo.id) {
        forge.view(&target, &pr.url)?
    } else if let Some(existing) = forge.find_existing(&target, branch, &repo.base_branch)? {
        existing
    } else {
        return Ok(SyncFetchResult::NoReviewObject);
    };

    Ok(SyncFetchResult::Summary {
        repo_index,
        summary,
    })
}

fn fetch_pr_summary_for_sync_from_artifact(
    cwd: &Path,
    bundle: &ChangeGroup,
    repo_index: usize,
    repo: &RepoEntry,
) -> Result<SyncFetchResult> {
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

    let summary = if let Some(pr) = publication_for_repo(bundle, &repo.id) {
        forge.view(&target, &pr.url)?
    } else if let Some(existing) = forge.find_existing(&target, branch, &repo.base_branch)? {
        existing
    } else {
        return Ok(SyncFetchResult::NoReviewObject);
    };

    Ok(SyncFetchResult::Summary {
        repo_index,
        summary,
    })
}

fn sync_pr_body_remote(
    active: &ActiveBundle,
    _repo_index: usize,
    repo: &RepoEntry,
) -> Result<SyncBodyResult> {
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    let forge = providers::for_repo(repo)?;
    let target = PrTarget::checkout(&cwd);
    let pr = publication_for_repo(&active.bundle, &repo.id)
        .with_context(|| format!("{}: no publication recorded after sync fetch", repo.id))?;
    let current_body = forge.view(&target, &pr.url)?.body.unwrap_or_default();
    let block = render_knit_pr_block(&active.bundle, Some(&repo.id));
    let next_body = upsert_knit_pr_block(&current_body, &block);
    if next_body == current_body {
        return Ok(SyncBodyResult::AlreadySynced);
    }
    forge.edit_body(&target, &pr.url, &next_body)?;
    Ok(SyncBodyResult::Synced(pr.url.clone()))
}

fn sync_pr_body_remote_from_artifact(
    cwd: &Path,
    bundle: &ChangeGroup,
    _repo_index: usize,
    repo: &RepoEntry,
) -> Result<SyncBodyResult> {
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
    let pr = publication_for_repo(bundle, &repo.id)
        .with_context(|| format!("{}: no publication recorded after sync fetch", repo.id))?;
    let current_body = forge.view(&target, &pr.url)?.body.unwrap_or_default();
    let block = render_knit_pr_block(bundle, Some(&repo.id));
    let next_body = upsert_knit_pr_block(&current_body, &block);
    if next_body == current_body {
        return Ok(SyncBodyResult::AlreadySynced);
    }
    forge.edit_body(&target, &pr.url, &next_body)?;
    Ok(SyncBodyResult::Synced(pr.url.clone()))
}

pub(super) fn sync_publications_for_indexes(
    active: &mut ActiveBundle,
    indexes: &[usize],
) -> Result<Vec<String>> {
    let jobs: Vec<(usize, RepoEntry)> = indexes
        .iter()
        .map(|&index| (index, active.bundle.repos[index].clone()))
        .collect();

    let active_read = &*active;
    let fetched: Vec<(String, Result<SyncFetchResult>)> = std::thread::scope(|scope| {
        let active_read = active_read;
        let handles: Vec<_> = jobs
            .iter()
            .map(|(repo_index, repo)| {
                let repo_index = *repo_index;
                let repo = repo.clone();
                let repo_id = repo.id.clone();
                scope.spawn(move || {
                    (
                        repo_id,
                        fetch_pr_summary_for_sync(active_read, repo_index, &repo),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("publish sync fetch thread panicked"))
            .collect()
    });

    let mut failures = Vec::new();
    let mut synced_repo_indexes = Vec::new();
    for (repo_id, result) in fetched {
        match result {
            Ok(SyncFetchResult::NoReviewObject) => {
                println!(
                    "{}: {}",
                    out::repo(&repo_id),
                    out::muted("no review object recorded")
                );
            }
            Ok(SyncFetchResult::Summary {
                repo_index,
                summary,
            }) => {
                let repo = active.bundle.repos[repo_index].clone();
                let forge = providers::for_repo(&repo)?;
                providers::upsert_publication(&mut active.bundle, &repo, forge.as_ref(), &summary);
                synced_repo_indexes.push(repo_index);
            }
            Err(error) => {
                println!("{}: {}", out::repo(&repo_id), out::danger("PR sync failed"));
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    if !synced_repo_indexes.is_empty() {
        save_active_bundle(active)?;
    }

    let active_read = &*active;
    let body_results: Vec<(String, Result<SyncBodyResult>)> = std::thread::scope(|scope| {
        let active_read = active_read;
        let handles: Vec<_> = synced_repo_indexes
            .iter()
            .map(|&repo_index| {
                let repo = active_read.bundle.repos[repo_index].clone();
                let repo_id = repo.id.clone();
                scope.spawn(move || (repo_id, sync_pr_body_remote(active_read, repo_index, &repo)))
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("publish sync body thread panicked"))
            .collect()
    });

    for (repo_id, result) in body_results {
        match result {
            Ok(SyncBodyResult::Synced(url)) => {
                println!(
                    "{}: {} {}",
                    out::repo(&repo_id),
                    out::movement("synced"),
                    url
                );
            }
            Ok(SyncBodyResult::AlreadySynced) => {
                println!(
                    "{}: {}",
                    out::repo(&repo_id),
                    out::muted("PR body already synced")
                );
            }
            Err(error) => {
                println!("{}: {}", out::repo(&repo_id), out::danger("PR sync failed"));
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    Ok(failures)
}

pub(super) fn sync_publications_for_indexes_from_artifact(
    cwd: &Path,
    bundle: &mut ChangeGroup,
    indexes: &[usize],
) -> Result<Vec<String>> {
    let jobs: Vec<(usize, RepoEntry)> = indexes
        .iter()
        .map(|&index| (index, bundle.repos[index].clone()))
        .collect();
    let bundle_snapshot = bundle.clone();

    let fetched: Vec<(String, Result<SyncFetchResult>)> = std::thread::scope(|scope| {
        let bundle = &bundle_snapshot;
        let handles: Vec<_> = jobs
            .iter()
            .map(|(repo_index, repo)| {
                let repo_index = *repo_index;
                let repo = repo.clone();
                let repo_id = repo.id.clone();
                scope.spawn(move || {
                    (
                        repo_id,
                        fetch_pr_summary_for_sync_from_artifact(cwd, bundle, repo_index, &repo),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("artifact publish sync fetch thread panicked")
            })
            .collect()
    });

    let mut failures = Vec::new();
    let mut synced_repo_indexes = Vec::new();
    for (repo_id, result) in fetched {
        match result {
            Ok(SyncFetchResult::NoReviewObject) => {
                println!(
                    "{}: {}",
                    out::repo(&repo_id),
                    out::muted("no review object recorded")
                );
            }
            Ok(SyncFetchResult::Summary {
                repo_index,
                summary,
            }) => {
                let repo = bundle.repos[repo_index].clone();
                let forge = providers::for_repo(&repo)?;
                providers::upsert_publication(bundle, &repo, forge.as_ref(), &summary);
                synced_repo_indexes.push(repo_index);
            }
            Err(error) => {
                println!("{}: {}", out::repo(&repo_id), out::danger("PR sync failed"));
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    let body_results: Vec<(String, Result<SyncBodyResult>)> = std::thread::scope(|scope| {
        let bundle_read = &*bundle;
        let handles: Vec<_> = synced_repo_indexes
            .iter()
            .map(|&repo_index| {
                let repo = bundle_read.repos[repo_index].clone();
                let repo_id = repo.id.clone();
                scope.spawn(move || {
                    (
                        repo_id,
                        sync_pr_body_remote_from_artifact(cwd, bundle_read, repo_index, &repo),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("artifact publish sync body thread panicked")
            })
            .collect()
    });

    for (repo_id, result) in body_results {
        match result {
            Ok(SyncBodyResult::Synced(url)) => {
                println!(
                    "{}: {} {}",
                    out::repo(&repo_id),
                    out::movement("synced"),
                    url
                );
            }
            Ok(SyncBodyResult::AlreadySynced) => {
                println!(
                    "{}: {}",
                    out::repo(&repo_id),
                    out::muted("PR body already synced")
                );
            }
            Err(error) => {
                println!("{}: {}", out::repo(&repo_id), out::danger("PR sync failed"));
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    Ok(failures)
}
