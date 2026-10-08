//! PR body cross-link sync: fetch each repo's live review object, record it
//! in the bundle, and upsert the managed Knit block into every PR body. The
//! `*_from_artifact` variants run without local checkouts.

use super::policy::ResolvedText;
use super::pr_body::{replace_prose, sync_knit_pr_body};
use crate::checkout::checkout_dir;
use crate::model::{AppliedText, ChangeGroup, Gate, RepoEntry};
use crate::output as out;
use crate::providers::{self, publication_for_repo, PullRequest};
use crate::store::{save_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

// One per repository in a bounded fetch, so the variant spread is not worth
// an extra allocation on the common path.
#[allow(clippy::large_enum_variant)]
enum SyncFetchResult {
    NoReviewObject,
    Summary {
        repo_index: usize,
        summary: PullRequest,
        /// `None` keeps the recorded gates when the host could not be read.
        gates: Option<Vec<Gate>>,
    },
}

enum SyncBodyResult {
    Synced(String),
    AlreadySynced,
}

struct SyncedText {
    result: SyncBodyResult,
    title: Option<String>,
    applied: Option<AppliedText>,
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
    let base = publication_for_repo(&active.bundle, &repo.id)
        .map(|p| p.base_branch.as_str())
        .unwrap_or(&repo.base_branch);
    let target = crate::contribution::target(&cwd, repo, forge.as_ref(), base, false)?;

    let mut target = target;
    if let Some(id) = &mut target.contribution {
        if let Some(p) = publication_for_repo(&active.bundle, &repo.id) {
            id.base = p.base_branch.clone();
        }
    }
    let summary = if let Some(pr) = publication_for_repo(&active.bundle, &repo.id) {
        forge.view(&target, &pr.url)?
    } else if let Some(existing) = forge.find_existing(
        &target,
        &crate::contribution::head(repo, branch)?,
        &repo.base_branch,
    )? {
        existing
    } else {
        return Ok(SyncFetchResult::NoReviewObject);
    };

    let gates = review_gates(forge.as_ref(), &target, &summary);
    Ok(SyncFetchResult::Summary {
        repo_index,
        summary,
        gates,
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
    let forge = providers::for_repo(repo)?;
    let base = publication_for_repo(bundle, &repo.id)
        .map(|p| p.base_branch.as_str())
        .unwrap_or(&repo.base_branch);
    let target = crate::contribution::target(cwd, repo, forge.as_ref(), base, true)?;

    let mut target = target;
    if let Some(id) = &mut target.contribution {
        if let Some(p) = publication_for_repo(bundle, &repo.id) {
            id.base = p.base_branch.clone();
        }
    }
    let summary = if let Some(pr) = publication_for_repo(bundle, &repo.id) {
        forge.view(&target, &pr.url)?
    } else if let Some(existing) = forge.find_existing(
        &target,
        &crate::contribution::head(repo, branch)?,
        &repo.base_branch,
    )? {
        existing
    } else {
        return Ok(SyncFetchResult::NoReviewObject);
    };

    let gates = review_gates(forge.as_ref(), &target, &summary);
    Ok(SyncFetchResult::Summary {
        repo_index,
        summary,
        gates,
    })
}

fn review_gates(
    forge: &dyn providers::Forge,
    target: &providers::PrTarget,
    summary: &PullRequest,
) -> Option<Vec<Gate>> {
    if summary.state.as_deref() != Some("OPEN") {
        return Some(Vec::new());
    }
    forge.gates(target, summary).ok()
}

fn print_gates(repo_id: &str, gates: &[Gate]) {
    if let Some(headline) = providers::gates::headline(gates) {
        println!("{}: {}", out::repo(repo_id), out::warn(&headline));
    }
}

fn sync_pr_body_remote(
    active: &ActiveBundle,
    repo: &RepoEntry,
    text: Option<&ResolvedText>,
) -> Result<SyncedText> {
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    let forge = providers::for_repo(repo)?;
    let base = publication_for_repo(&active.bundle, &repo.id)
        .map(|p| p.base_branch.as_str())
        .unwrap_or(&repo.base_branch);
    let target = crate::contribution::target(&cwd, repo, forge.as_ref(), base, false)?;
    let pr = publication_for_repo(&active.bundle, &repo.id)
        .with_context(|| format!("{}: no publication recorded after sync fetch", repo.id))?;
    let mut target = target;
    if let Some(id) = &mut target.contribution {
        id.base = pr.base_branch.clone();
    }
    let current = forge.view(&target, &pr.url)?;
    let current_body = current.body.clone().unwrap_or_default();
    let open = current
        .state
        .as_deref()
        .is_none_or(|s| matches!(s.to_ascii_lowercase().as_str(), "open" | "opened"));
    let previous = pr.applied.clone().unwrap_or_default();
    let mut applied = previous.clone();
    let mut title = None;
    let mut source = current_body.clone();
    if let Some(text) = text.filter(|_| open) {
        let wanted = super::policy::applied(
            text.title_chosen,
            &text.title,
            text.body_from_file,
            &text.body,
        );
        if wanted.title.is_some() && wanted.title != previous.title {
            if current.title.as_deref() != wanted.title.as_deref() {
                title = wanted.title.clone();
            }
            applied.title = wanted.title;
        }
        if wanted.body_sha256.is_some() && wanted.body_sha256 != previous.body_sha256 {
            source = replace_prose(&current_body, &text.body);
            applied.body_sha256 = wanted.body_sha256;
        }
    }
    let next_body = sync_knit_pr_body(&active.bundle, &repo.id, forge.id(), &source);
    if let Some(title) = &title {
        forge.edit_title(&target, &pr.url, title)?;
    }
    if next_body != current_body {
        forge.edit_body(&target, &pr.url, &next_body)?;
    }
    Ok(SyncedText {
        result: if title.is_none() && next_body == current_body {
            SyncBodyResult::AlreadySynced
        } else {
            SyncBodyResult::Synced(pr.url.clone())
        },
        title,
        applied: (applied != previous).then_some(applied),
    })
}

fn sync_pr_body_remote_from_artifact(
    cwd: &Path,
    bundle: &ChangeGroup,
    _repo_index: usize,
    repo: &RepoEntry,
) -> Result<SyncBodyResult> {
    let forge = providers::for_repo(repo)?;
    let base = publication_for_repo(bundle, &repo.id)
        .map(|p| p.base_branch.as_str())
        .unwrap_or(&repo.base_branch);
    let target = crate::contribution::target(cwd, repo, forge.as_ref(), base, true)?;
    let pr = publication_for_repo(bundle, &repo.id)
        .with_context(|| format!("{}: no publication recorded after sync fetch", repo.id))?;
    let mut target = target;
    if let Some(id) = &mut target.contribution {
        id.base = pr.base_branch.clone();
    }
    let current_body = forge.view(&target, &pr.url)?.body.unwrap_or_default();
    let next_body = sync_knit_pr_body(bundle, &repo.id, forge.id(), &current_body);
    if next_body == current_body {
        return Ok(SyncBodyResult::AlreadySynced);
    }
    forge.edit_body(&target, &pr.url, &next_body)?;
    Ok(SyncBodyResult::Synced(pr.url.clone()))
}

pub(super) fn sync_publications_for_indexes(
    active: &mut ActiveBundle,
    indexes: &[usize],
    texts: &BTreeMap<String, ResolvedText>,
) -> Result<Vec<String>> {
    let jobs: Vec<(usize, RepoEntry)> = indexes
        .iter()
        .map(|&index| (index, active.bundle.repos[index].clone()))
        .collect();

    let active_read = &*active;
    let fetched: Vec<(String, Result<SyncFetchResult>)> = std::thread::scope(|scope| {
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
                gates,
            }) => {
                let repo = active.bundle.repos[repo_index].clone();
                let forge = providers::for_repo(&repo)?;
                providers::upsert_publication(&mut active.bundle, &repo, forge.as_ref(), &summary);
                if let Some(gates) = gates {
                    print_gates(&repo.id, &gates);
                    providers::record_gates(&mut active.bundle, &repo.id, gates);
                }
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
    let body_results: Vec<(String, Result<SyncedText>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = synced_repo_indexes
            .iter()
            .map(|&repo_index| {
                let repo = active_read.bundle.repos[repo_index].clone();
                let repo_id = repo.id.clone();
                let text = texts.get(&repo_id);
                scope.spawn(move || (repo_id, sync_pr_body_remote(active_read, &repo, text)))
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("publish sync body thread panicked"))
            .collect()
    });

    let mut recorded = false;
    for (repo_id, result) in body_results {
        match result {
            Ok(synced) => {
                match &synced.result {
                    SyncBodyResult::Synced(url) => println!(
                        "{}: {} {}",
                        out::repo(&repo_id),
                        out::movement("synced"),
                        url
                    ),
                    SyncBodyResult::AlreadySynced => println!(
                        "{}: {}",
                        out::repo(&repo_id),
                        out::muted("PR body already synced")
                    ),
                }
                if let Some(title) = &synced.title {
                    println!("{}: {} {title}", out::repo(&repo_id), out::muted("title"));
                }
                if synced.title.is_some() || synced.applied.is_some() {
                    if let Some(publication) = active
                        .bundle
                        .publications
                        .iter_mut()
                        .rev()
                        .find(|p| p.repo_id == repo_id)
                    {
                        if let Some(title) = synced.title {
                            publication.title = Some(title);
                        }
                        if let Some(applied) = synced.applied {
                            publication.applied = Some(applied);
                        }
                        recorded = true;
                    }
                }
            }
            Err(error) => {
                println!("{}: {}", out::repo(&repo_id), out::danger("PR sync failed"));
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }
    if recorded {
        save_active_bundle(active)?;
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
                gates,
            }) => {
                let repo = bundle.repos[repo_index].clone();
                let forge = providers::for_repo(&repo)?;
                providers::upsert_publication(bundle, &repo, forge.as_ref(), &summary);
                if let Some(gates) = gates {
                    print_gates(&repo.id, &gates);
                    providers::record_gates(bundle, &repo.id, gates);
                }
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
