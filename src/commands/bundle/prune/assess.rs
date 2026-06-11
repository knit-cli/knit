//! Per-bundle prune assessment: scan every bundle's repos for open/merged
//! review objects, pending checkout changes, and unpublished feature-branch
//! commits, with a shared cache so parallel scans hit each host PR and
//! checkout only once.

use super::print_prune_warning;
use crate::checkout::is_in_place;
use crate::git::{git_output, is_git_worktree, ref_exists, resolve_base_ref};
use crate::model::{ChangeGroup, RepoEntry};
use crate::providers::{self, Forge, PrTarget, PullRequest};
use crate::store::{read_json, write_json};
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Uncommitted work found in a checkout, split by whether Git tracks it.
#[derive(Clone, Copy, Default)]
pub(super) struct Pending {
    pub(super) tracked: bool,
    pub(super) untracked: bool,
}

impl Pending {
    pub(super) fn any(self) -> bool {
        self.tracked || self.untracked
    }

    pub(super) fn merge(&mut self, other: Pending) {
        self.tracked |= other.tracked;
        self.untracked |= other.untracked;
    }
}

/// Everything prune learned about one bundle, so the same signals drive the
/// prune decision, the `--untracked` relaxation, and the `--report` view.
pub(super) struct PruneAssessment {
    pub(super) id: String,
    pub(super) repo_count: usize,
    pub(super) saw_publication: bool,
    pub(super) saw_open_publication: bool,
    pub(super) saw_merged_publication: bool,
    pub(super) saw_unpublished_commits: bool,
    pub(super) pending: Pending,
}

impl PruneAssessment {
    /// Reason this bundle is dead work, or `None` if it should be kept.
    /// With `untracked` set, checkouts whose only uncommitted work is
    /// untracked files no longer hold the bundle back.
    pub(super) fn candidate_reason(&self, untracked: bool) -> Option<String> {
        if self.saw_open_publication || self.pending.tracked || self.saw_unpublished_commits {
            return None;
        }
        if self.pending.untracked && !untracked {
            return None;
        }
        let base = if self.saw_merged_publication {
            "recorded PRs are merged"
        } else if self.saw_publication {
            "no open PRs and no pending changes"
        } else {
            "no recorded PRs and no pending changes"
        };
        if self.pending.untracked {
            Some(format!("{base}; discards untracked files"))
        } else {
            Some(base.to_string())
        }
    }

    /// True when the bundle would be dead work but for untracked files alone.
    pub(super) fn blocked_by_untracked_only(&self) -> bool {
        !self.saw_open_publication
            && !self.saw_unpublished_commits
            && !self.pending.tracked
            && self.pending.untracked
    }

    /// The PR side of why the bundle is (or is not yet) dead work.
    pub(super) fn pr_basis(&self) -> &'static str {
        if self.saw_open_publication {
            "open PR(s)"
        } else if self.saw_merged_publication {
            "recorded PRs are merged"
        } else if self.saw_publication {
            "no open PRs"
        } else {
            "no recorded PRs"
        }
    }
}

#[derive(Clone)]
pub(super) struct PruneCache {
    pr_by_url: Arc<Mutex<HashMap<String, PullRequest>>>,
    pr_by_branch: Arc<Mutex<HashMap<BranchKey, Option<PullRequest>>>>,
    pending_changes: Arc<Mutex<HashMap<String, Pending>>>,
    gh_auth_failure: Arc<Mutex<bool>>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct BranchKey {
    repo_path: String,
    head: String,
    base: String,
}

impl PruneCache {
    pub(super) fn new() -> Self {
        Self {
            pr_by_url: Arc::new(Mutex::new(HashMap::new())),
            pr_by_branch: Arc::new(Mutex::new(HashMap::new())),
            pending_changes: Arc::new(Mutex::new(HashMap::new())),
            gh_auth_failure: Arc::new(Mutex::new(false)),
        }
    }

    pub(super) fn note_refresh_failure(
        &self,
        bundle_id: &str,
        repo_id: &str,
        context: &str,
        err: &anyhow::Error,
    ) {
        if providers::is_likely_host_auth_failure(err) {
            let mut seen = self.gh_auth_failure.lock().unwrap();
            if !*seen {
                print_prune_warning(format!(
                    "GitHub auth failed during prune refresh ({err:#}). Further refresh warnings are suppressed; using last recorded PR state."
                ));
                *seen = true;
            }
            return;
        }
        print_prune_warning(format!(
            "{bundle_id}/{repo_id}: {context} ({err:#}); using last recorded state"
        ));
    }

    pub(super) fn view_pr(&self, forge: &dyn Forge, cwd: &Path, url: &str) -> Result<PullRequest> {
        {
            let cache = self.pr_by_url.lock().unwrap();
            if let Some(pr) = cache.get(url) {
                return Ok(pr.clone());
            }
        }
        let pr = forge.view(&PrTarget::checkout(cwd), url)?;
        self.pr_by_url
            .lock()
            .unwrap()
            .insert(url.to_string(), pr.clone());
        Ok(pr)
    }

    pub(super) fn find_existing_pr(
        &self,
        forge: &dyn Forge,
        cwd: &Path,
        branch: &str,
        base_branch: &str,
    ) -> Result<Option<PullRequest>> {
        let key = BranchKey {
            repo_path: cwd.to_string_lossy().to_string(),
            head: branch.to_string(),
            base: base_branch.to_string(),
        };
        {
            let cache = self.pr_by_branch.lock().unwrap();
            if let Some(result) = cache.get(&key) {
                return Ok(result.clone());
            }
        }
        let result = forge.find_existing(&PrTarget::checkout(cwd), branch, base_branch)?;
        self.pr_by_branch
            .lock()
            .unwrap()
            .insert(key, result.clone());
        Ok(result)
    }

    pub(super) fn checkout_has_pending_changes(
        &self,
        root: &Path,
        repo: &RepoEntry,
    ) -> Result<Pending> {
        let Some(path) = checkout_path(root, repo) else {
            return Ok(Pending::default());
        };
        let key = path.to_string_lossy().to_string();
        {
            let cache = self.pending_changes.lock().unwrap();
            if let Some(&result) = cache.get(&key) {
                return Ok(result);
            }
        }
        let result = path_pending_changes(&path)?;
        self.pending_changes.lock().unwrap().insert(key, result);
        Ok(result)
    }
}

/// Assess every bundle, returning the assessments plus the set of ids that exist
/// locally. Best-effort: an unreadable bundle file is skipped with a warning, and
/// a bundle that fails its scan is skipped rather than aborting the whole prune.
pub(super) fn assess_bundles(
    root: &Path,
    entries: Vec<PathBuf>,
    refresh: bool,
    cache: &PruneCache,
) -> (Vec<PruneAssessment>, BTreeSet<String>) {
    let mut local_ids = BTreeSet::new();
    let mut bundles = Vec::new();
    for path in entries {
        match read_json::<ChangeGroup>(&path) {
            Ok(bundle) => {
                local_ids.insert(bundle.id.clone());
                bundles.push((path, bundle));
            }
            Err(err) => print_prune_warning(format!(
                "skipped unreadable bundle {}: {err:#}",
                path.display()
            )),
        }
    }

    let results: Vec<(String, Result<PruneAssessment>)> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (path, mut bundle) in bundles {
            handles.push(scope.spawn(move || {
                let id = bundle.id.clone();
                (id, assess_bundle(root, &path, &mut bundle, refresh, cache))
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });

    let mut assessments = Vec::new();
    for (id, result) in results {
        match result {
            Ok(assessment) => assessments.push(assessment),
            Err(err) => print_prune_warning(format!("{id}: skipped during prune scan: {err:#}")),
        }
    }
    (assessments, local_ids)
}

fn assess_bundle(
    root: &Path,
    path: &Path,
    bundle: &mut ChangeGroup,
    refresh: bool,
    cache: &PruneCache,
) -> Result<PruneAssessment> {
    let bundle_id = bundle.id.clone();
    let jobs: Vec<(usize, RepoEntry, Option<crate::model::PublicationEntry>)> = bundle
        .repos
        .iter()
        .enumerate()
        .map(|(index, repo)| {
            (
                index,
                repo.clone(),
                providers::publication_for_repo(bundle, &repo.id).cloned(),
            )
        })
        .collect();

    let repo_results: Vec<(usize, Result<RepoPruneSignals>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|(index, repo, recorded)| {
                let index = *index;
                let repo = repo.clone();
                let recorded = recorded.clone();
                let bundle_id = bundle_id.clone();
                scope.spawn(move || {
                    (
                        index,
                        assess_repo_signals(
                            root,
                            &bundle_id,
                            &repo,
                            recorded.as_ref(),
                            refresh,
                            cache,
                        ),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("prune repo worker thread panicked"))
            .collect()
    });

    let mut saw_publication = false;
    let mut saw_merged_publication = false;
    let mut saw_open_publication = false;
    let mut saw_unpublished_commits = false;
    let mut pending = Pending::default();

    for (index, result) in repo_results {
        let repo_id = bundle.repos[index].id.clone();
        let signals = result.with_context(|| format!("{bundle_id}/{repo_id}"))?;
        if let Some(pr) = signals.publication_update {
            let repo = bundle.repos[index].clone();
            if let Ok(forge) = providers::for_repo(&repo) {
                providers::upsert_publication(bundle, &repo, forge.as_ref(), &pr);
            }
        }
        pending.merge(signals.pending);
        if signals.pending_check_failed {
            pending.tracked = true;
        }
        saw_publication |= signals.saw_publication;
        saw_open_publication |= signals.saw_open_publication;
        saw_merged_publication |= signals.saw_merged_publication;
        saw_unpublished_commits |= signals.unpublished_commits;
    }

    if refresh {
        write_json(path, bundle)?;
    }

    Ok(PruneAssessment {
        id: bundle.id.clone(),
        repo_count: bundle.repos.len(),
        saw_publication,
        saw_open_publication,
        saw_merged_publication,
        saw_unpublished_commits,
        pending,
    })
}

struct RepoPruneSignals {
    publication_update: Option<PullRequest>,
    pub(super) pending: Pending,
    pending_check_failed: bool,
    pub(super) saw_publication: bool,
    pub(super) saw_open_publication: bool,
    pub(super) saw_merged_publication: bool,
    unpublished_commits: bool,
}

fn assess_repo_signals(
    root: &Path,
    bundle_id: &str,
    repo: &RepoEntry,
    recorded: Option<&crate::model::PublicationEntry>,
    refresh: bool,
    cache: &PruneCache,
) -> Result<RepoPruneSignals> {
    let branch = repo.feature_branch.as_deref();
    let mut publication_update = None;

    if refresh {
        if let Ok(forge) = providers::for_repo(repo) {
            if let Some(existing) = recorded {
                match cache.view_pr(forge.as_ref(), Path::new(&repo.path), &existing.url) {
                    Ok(pr) => publication_update = Some(pr),
                    Err(err) => cache.note_refresh_failure(
                        bundle_id,
                        &repo.id,
                        &format!("could not refresh {}", existing.url),
                        &err,
                    ),
                }
            } else if let Some(branch) = branch {
                match cache.find_existing_pr(
                    forge.as_ref(),
                    Path::new(&repo.path),
                    branch,
                    &repo.base_branch,
                ) {
                    Ok(Some(pr)) => publication_update = Some(pr),
                    Ok(None) => {}
                    Err(err) => cache.note_refresh_failure(
                        bundle_id,
                        &repo.id,
                        &format!("could not check for an open review object on {branch}"),
                        &err,
                    ),
                }
            }
        }
    }

    let (saw_publication, saw_open_publication, saw_merged_publication) =
        if let Some(pr) = publication_update.as_ref() {
            publication_flags_from_pr(
                branch,
                pr.head_ref_name.as_deref().unwrap_or(""),
                pr.state.as_deref().unwrap_or("UNKNOWN"),
            )
        } else if let Some(existing) = recorded {
            publication_flags_from_publication(branch, existing)
        } else {
            (false, false, false)
        };

    let (pending, pending_check_failed) = match cache.checkout_has_pending_changes(root, repo) {
        Ok(found) => (found, false),
        Err(err) => {
            print_prune_warning(format!(
                "{bundle_id}/{}: could not inspect checkout for pending changes ({err:#}); keeping the bundle to be safe",
                repo.id
            ));
            (Pending::default(), true)
        }
    };

    // Committed work without a review object is unpublished work, not dead
    // work: a clean checkout says nothing about commits already recorded on
    // the feature branch (locally or pushed by another user). Only a repo
    // with no publication at all needs this guard — once a PR exists its
    // state, not the branch shape, decides liveness.
    let unpublished_commits = if saw_publication {
        false
    } else {
        match feature_branch_unmerged_commits(repo) {
            Ok(found) => found,
            Err(err) => {
                print_prune_warning(format!(
                    "{bundle_id}/{}: could not inspect feature branch for unpublished commits ({err:#}); keeping the bundle to be safe",
                    repo.id
                ));
                true
            }
        }
    };

    Ok(RepoPruneSignals {
        publication_update,
        pending,
        pending_check_failed,
        saw_publication,
        saw_open_publication,
        saw_merged_publication,
        unpublished_commits,
    })
}

/// True when the bundle's feature branch (the local branch or its `origin/`
/// counterpart in the source repo) carries commits the base branch does not.
fn feature_branch_unmerged_commits(repo: &RepoEntry) -> Result<bool> {
    let Some(branch) = repo.feature_branch.as_deref() else {
        return Ok(false);
    };
    let repo_root = Path::new(&repo.path);
    if !repo_root.exists() {
        return Ok(false);
    }
    let base_ref = resolve_base_ref(repo_root, &repo.base_branch);
    for candidate in [branch.to_string(), format!("origin/{branch}")] {
        if !ref_exists(repo_root, &candidate) {
            continue;
        }
        let range = format!("{base_ref}..{candidate}");
        let count = git_output(repo_root, ["rev-list", "--count", &range])
            .with_context(|| format!("failed to count commits in {range}"))?;
        if count.trim() != "0" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn publication_flags_from_publication(
    branch: Option<&str>,
    publication: &crate::model::PublicationEntry,
) -> (bool, bool, bool) {
    publication_flags_from_pr(branch, &publication.head_branch, &publication.state)
}

fn publication_flags_from_pr(
    branch: Option<&str>,
    head_branch: &str,
    state: &str,
) -> (bool, bool, bool) {
    if Some(head_branch) != branch {
        return (true, true, false);
    }
    if publication_state_is_merged(state) {
        (true, false, true)
    } else if publication_state_is_closed(state) {
        (true, false, false)
    } else {
        (true, true, false)
    }
}

pub(super) fn publication_state_is_merged(state: &str) -> bool {
    state.eq_ignore_ascii_case("merged")
}

pub(super) fn publication_state_is_closed(state: &str) -> bool {
    state.eq_ignore_ascii_case("closed")
}

fn _checkout_has_pending_changes(root: &Path, repo: &RepoEntry) -> Result<bool> {
    let Some(path) = checkout_path(root, repo) else {
        return Ok(false);
    };
    Ok(path_pending_changes(&path)?.any())
}

fn checkout_path(root: &Path, repo: &RepoEntry) -> Option<PathBuf> {
    if is_in_place(repo) {
        return Some(PathBuf::from(&repo.path));
    }
    repo.worktree_path
        .as_deref()
        .map(|path| resolve_path(root, path))
}

fn resolve_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

pub(super) fn path_pending_changes(path: &Path) -> Result<Pending> {
    if !path.exists() {
        return Ok(Pending::default());
    }
    if is_git_worktree(path) {
        let status = git_output(path, ["status", "--porcelain"])?;
        let mut pending = Pending::default();
        for line in status.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with("??") {
                pending.untracked = true;
            } else {
                pending.tracked = true;
            }
        }
        return Ok(pending);
    }
    // Stray files outside a Git worktree can't be classified, so treat them
    // as tracked changes: they block pruning even with --untracked.
    if path.is_file() {
        return Ok(Pending {
            tracked: true,
            untracked: false,
        });
    }
    let mut pending = Pending::default();
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        pending.merge(path_pending_changes(&entry?.path())?);
        if pending.tracked && pending.untracked {
            break;
        }
    }
    Ok(pending)
}
