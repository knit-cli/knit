//! Per-bundle prune assessment: scan every bundle's repos for open/merged
//! review objects, pending checkout changes, and unpublished feature-branch
//! commits, with a shared cache so parallel scans hit each host PR and
//! checkout only once.

use super::print_prune_warning;
use crate::checkout::is_in_place;
use crate::model::{ChangeGroup, RepoEntry};
use crate::pending::feature_branch_unmerged_commits;
pub(super) use crate::pending::{path_pending_changes, Pending};
use crate::providers::{self, Forge, PrTarget, PullRequest};
use crate::store::{read_json, write_json};
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Everything prune learned about one bundle, so the same signals drive the
/// prune decision, the `--untracked` relaxation, and the `--report` view.
pub(super) struct PruneAssessment {
    pub(super) id: String,
    pub(super) status: crate::commands::bundle::BundleStatus,
    pub(super) repo_count: usize,
    pub(super) saw_publication: bool,
    pub(super) saw_open_publication: bool,
    pub(super) saw_merged_publication: bool,
    pub(super) saw_unpublished_commits: bool,
    /// The bundle's last landing went to an intermediate destination (a
    /// staging lane), so its reviews are merged but its work is still in
    /// flight towards a terminal destination.
    pub(super) landed_intermediate: bool,
    pub(super) pending: Pending,
    /// False for finished (landed/archived) bundles kept as history without
    /// scanning: their signal fields are defaults, not observations.
    pub(super) assessed: bool,
}

impl PruneAssessment {
    /// A finished (landed/archived) bundle kept as history without scanning
    /// its checkouts or refreshing its recorded reviews.
    fn finished(bundle: &ChangeGroup, status: crate::commands::bundle::BundleStatus) -> Self {
        Self {
            id: bundle.id.clone(),
            status,
            repo_count: bundle.repos.len(),
            saw_publication: false,
            saw_open_publication: false,
            saw_merged_publication: false,
            saw_unpublished_commits: false,
            landed_intermediate: landed_intermediate(bundle),
            pending: Pending::default(),
            assessed: false,
        }
    }

    /// Reason this bundle is dead work, or `None` if it should be kept.
    /// With `untracked` set, checkouts whose only uncommitted work is
    /// untracked files no longer hold the bundle back.
    pub(super) fn candidate_reason(&self, untracked: bool) -> Option<String> {
        if self.saw_open_publication || self.pending.tracked || self.saw_unpublished_commits {
            return None;
        }
        // Merged reviews normally mean the work is over. After a landing into
        // an intermediate destination they mean the opposite: the bundle
        // reached staging and is waiting for its next destination.
        if self.landed_intermediate {
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
        } else if self.landed_intermediate {
            "landed into an intermediate destination"
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
    /// Forges whose credentials already failed this run. Auth failures repeat
    /// identically for every PR on that host, so each forge warns once and
    /// its remaining refreshes are skipped instead of re-failing per repo.
    auth_failed_forges: Arc<Mutex<BTreeSet<String>>>,
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
            auth_failed_forges: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub(super) fn note_refresh_failure(
        &self,
        forge_id: &str,
        bundle_id: &str,
        repo_id: &str,
        context: &str,
        err: &anyhow::Error,
    ) {
        if providers::is_likely_host_auth_failure(err) {
            let mut failed = self.auth_failed_forges.lock().unwrap();
            if failed.insert(forge_id.to_string()) {
                print_prune_warning(format!(
                    "{forge_id}: authentication failed during prune refresh ({err:#}). Skipping further {forge_id} refreshes; using last recorded review state."
                ));
            }
            return;
        }
        print_prune_warning(format!(
            "{bundle_id}/{repo_id}: {context} ({err:#}); using last recorded state"
        ));
    }

    pub(super) fn forge_auth_failed(&self, forge_id: &str) -> bool {
        self.auth_failed_forges.lock().unwrap().contains(forge_id)
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
///
/// Finished (landed/archived) bundles are kept as history without scanning
/// unless `include_finished` opts them into pruning: they can never become
/// candidates otherwise, so inspecting their checkouts and refreshing their
/// recorded reviews — often on hosts this machine has no credentials for —
/// would be wasted work that only produces warnings.
pub(super) fn assess_bundles(
    root: &Path,
    entries: Vec<PathBuf>,
    refresh: bool,
    include_finished: bool,
    cache: &PruneCache,
) -> Result<(Vec<PruneAssessment>, BTreeSet<String>)> {
    let mut local_ids = BTreeSet::new();
    let mut assessments = Vec::new();
    let mut jobs = Vec::new();
    for path in entries {
        match read_json::<ChangeGroup>(&path) {
            Ok(bundle) => {
                local_ids.insert(bundle.id.clone());
                let status = crate::commands::bundle::bundle_state(&bundle);
                if !include_finished
                    && matches!(
                        status,
                        crate::commands::bundle::BundleStatus::Landed
                            | crate::commands::bundle::BundleStatus::Archived
                    )
                {
                    assessments.push(PruneAssessment::finished(&bundle, status));
                    continue;
                }
                jobs.push((path, Mutex::new(bundle)));
            }
            Err(err) => print_prune_warning(format!(
                "skipped unreadable bundle {}: {err:#}",
                path.display()
            )),
        }
    }

    // Refresh scans are bound by forge API reads, plain scans by local git
    // work; either way the pool is a deliberate size, not one thread per
    // bundle — hundreds of simultaneous `gh` processes can fail spuriously
    // (and read like an auth outage) long before the host rate limit does.
    let limit = if refresh {
        crate::parallel::forge_jobs()?
    } else {
        crate::parallel::git_jobs()?
    };
    let results: Mutex<Vec<(String, Result<PruneAssessment>)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        crate::parallel::spawn_bounded(scope, &jobs, limit, |(path, bundle)| {
            let mut bundle = bundle.lock().unwrap();
            let id = bundle.id.clone();
            let result = assess_bundle(root, path, &mut bundle, refresh, cache);
            results.lock().unwrap().push((id, result));
        });
    });

    for (id, result) in results.into_inner().unwrap() {
        match result {
            Ok(assessment) => assessments.push(assessment),
            Err(err) => print_prune_warning(format!("{id}: skipped during prune scan: {err:#}")),
        }
    }
    // Workers finish in whatever order the hosts answer; the listing should
    // not change shape between runs because of that.
    assessments.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((assessments, local_ids))
}

fn assess_bundle(
    root: &Path,
    path: &Path,
    bundle: &mut ChangeGroup,
    refresh: bool,
    cache: &PruneCache,
) -> Result<PruneAssessment> {
    let bundle_id = bundle.id.clone();
    let jobs: Vec<(RepoEntry, Option<crate::model::PublicationEntry>)> = bundle
        .repos
        .iter()
        .map(|repo| {
            (
                repo.clone(),
                providers::publication_for_repo(bundle, &repo.id).cloned(),
            )
        })
        .collect();

    let mut saw_publication = false;
    let mut saw_merged_publication = false;
    let mut saw_open_publication = false;
    let mut saw_unpublished_commits = false;
    let mut pending = Pending::default();
    let mut changed = false;

    for (repo, recorded) in jobs {
        let signals =
            assess_repo_signals(root, &bundle_id, &repo, recorded.as_ref(), refresh, cache)
                .with_context(|| format!("{bundle_id}/{}", repo.id))?;
        if let Some(pr) = signals.publication_update {
            if let Ok(forge) = providers::for_repo(&repo) {
                changed |= providers::upsert_publication(bundle, &repo, forge.as_ref(), &pr);
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

    if changed {
        write_json(path, bundle)?;
    }

    Ok(PruneAssessment {
        id: bundle.id.clone(),
        status: crate::commands::bundle::bundle_state(bundle),
        repo_count: bundle.repos.len(),
        saw_publication,
        saw_open_publication,
        saw_merged_publication,
        saw_unpublished_commits,
        landed_intermediate: landed_intermediate(bundle),
        pending,
        assessed: true,
    })
}

/// Whether the bundle's most recent landing left it open on purpose.
fn landed_intermediate(bundle: &ChangeGroup) -> bool {
    bundle
        .nodes
        .iter()
        .rev()
        .find(|node| node.node_type == "feature.landed")
        .and_then(|node| node.landing.as_ref())
        .is_some_and(|landing| !landing.terminal)
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
            if cache.forge_auth_failed(forge.id()) {
                // This forge already rejected our credentials; every further
                // call would fail the same way, so stay on recorded state.
            } else if let Some(existing) = recorded {
                match cache.view_pr(forge.as_ref(), Path::new(&repo.path), &existing.url) {
                    Ok(pr) => publication_update = Some(pr),
                    Err(err) => cache.note_refresh_failure(
                        forge.id(),
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
                        forge.id(),
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
