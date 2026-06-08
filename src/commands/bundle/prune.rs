//! `knit prune` — find and delete dead-work bundles, orphan worktrees, and
//! orphaned KnitHub remote bundle records.
//!
//! A bundle is "dead work" when it has no open PRs and no uncommitted tracked
//! changes in any checkout. The same per-bundle signals drive the prune
//! decision, the `--untracked` relaxation, and the `--report` view.

use super::{bundle_json_paths, current_root, delete_bundle};
use crate::checkout::is_in_place;
use crate::git::{git_output, is_git_worktree};
use crate::model::{ChangeGroup, KnitConfig, RepoEntry};
use crate::output as out;
use crate::providers::{self, Forge, PrTarget, PullRequest};
use crate::store::{bundle_exists, read_json, write_json};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct PruneCandidate {
    id: String,
    repo_count: usize,
    reason: String,
}

struct OrphanWorktree {
    id: String,
    path: PathBuf,
    discards_pending: bool,
}

/// A bundle that exists on the KnitHub sync remote but has no local artifact and
/// whose recorded pull requests are all merged or closed, so prune can never reach
/// it through the local `.knit/bundles/` scan.
struct RemoteOrphan {
    remote_id: String,
    slug: String,
    reason: &'static str,
}

/// Surface a non-fatal prune problem on stderr without aborting the whole run.
fn print_prune_warning(message: impl std::fmt::Display) {
    eprintln!("{}", out::warn(message));
}

/// Uncommitted work found in a checkout, split by whether Git tracks it.
#[derive(Clone, Copy, Default)]
struct Pending {
    tracked: bool,
    untracked: bool,
}

impl Pending {
    fn any(self) -> bool {
        self.tracked || self.untracked
    }

    fn merge(&mut self, other: Pending) {
        self.tracked |= other.tracked;
        self.untracked |= other.untracked;
    }
}

/// Everything prune learned about one bundle, so the same signals drive the
/// prune decision, the `--untracked` relaxation, and the `--report` view.
struct PruneAssessment {
    id: String,
    repo_count: usize,
    saw_publication: bool,
    saw_open_publication: bool,
    saw_merged_publication: bool,
    pending: Pending,
}

impl PruneAssessment {
    /// Reason this bundle is dead work, or `None` if it should be kept.
    /// With `untracked` set, checkouts whose only uncommitted work is
    /// untracked files no longer hold the bundle back.
    fn candidate_reason(&self, untracked: bool) -> Option<String> {
        if self.saw_open_publication || self.pending.tracked {
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
    fn blocked_by_untracked_only(&self) -> bool {
        !self.saw_open_publication && !self.pending.tracked && self.pending.untracked
    }

    /// The PR side of why the bundle is (or is not yet) dead work.
    fn pr_basis(&self) -> &'static str {
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
struct PruneCache {
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
    fn new() -> Self {
        Self {
            pr_by_url: Arc::new(Mutex::new(HashMap::new())),
            pr_by_branch: Arc::new(Mutex::new(HashMap::new())),
            pending_changes: Arc::new(Mutex::new(HashMap::new())),
            gh_auth_failure: Arc::new(Mutex::new(false)),
        }
    }

    fn note_refresh_failure(
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

    fn view_pr(&self, forge: &dyn Forge, cwd: &Path, url: &str) -> Result<PullRequest> {
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

    fn find_existing_pr(
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

    fn checkout_has_pending_changes(&self, root: &Path, repo: &RepoEntry) -> Result<Pending> {
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
        self.pending_changes
            .lock()
            .unwrap()
            .insert(key, result);
        Ok(result)
    }
}

pub fn prune_merged_bundles(
    apply: bool,
    refresh: bool,
    untracked: bool,
    report: bool,
    worktrees: bool,
    force: bool,
    branches: bool,
    force_branches: bool,
    remote_branches: bool,
    remote_bundles: bool,
) -> Result<()> {
    if force_branches && !branches {
        bail!("Use --branches with --force-branches.");
    }
    if remote_branches && !branches {
        bail!("Use --branches with --remote-branches.");
    }
    if branches && !worktrees {
        bail!(
            "Pruning local branches requires --worktrees so generated checkouts are removed first."
        );
    }

    let root = current_root()?;
    let config = if remote_bundles {
        Some(crate::store::load_effective_config(&root)?)
    } else {
        None
    };
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        println!("{}", out::muted("No bundles."));
        return Ok(());
    }

    let mut entries = bundle_json_paths(&dir)?;
    entries.sort();
    let cache = PruneCache::new();
    let (assessments, local_ids) = assess_bundles(&root, entries, refresh, &cache);

    let mut candidates = Vec::new();
    let mut blocked_untracked = Vec::new();
    for assessment in &assessments {
        if let Some(reason) = assessment.candidate_reason(untracked) {
            candidates.push(PruneCandidate {
                id: assessment.id.clone(),
                repo_count: assessment.repo_count,
                reason,
            });
        } else if assessment.blocked_by_untracked_only() {
            blocked_untracked.push(assessment);
        }
    }

    let (orphan_worktrees, blocked_orphan_worktrees) = if worktrees {
        orphan_worktree_candidates(&root, force)?
    } else {
        (Vec::new(), Vec::new())
    };
    let remote_orphans = if remote_bundles {
        remote_orphan_candidates(config.as_ref(), &local_ids, &root, refresh)
    } else {
        Vec::new()
    };

    if report {
        print_prune_report(&assessments, untracked);
    }

    if candidates.is_empty()
        && orphan_worktrees.is_empty()
        && blocked_untracked.is_empty()
        && blocked_orphan_worktrees.is_empty()
        && remote_orphans.is_empty()
    {
        println!(
            "{}",
            out::muted("No dead bundles, orphan worktrees, or remote orphan records to prune.")
        );
        return Ok(());
    }

    if !candidates.is_empty() {
        println!("{}", out::heading("Dead bundle candidates:"));
        for candidate in &candidates {
            println!(
                "  {} {} repo(s), {}",
                out::node(&candidate.id),
                candidate.repo_count,
                out::muted(&candidate.reason)
            );
        }
    }

    if !blocked_untracked.is_empty() {
        println!(
            "{}",
            out::heading("Blocked by untracked files (use --untracked to prune):")
        );
        for assessment in &blocked_untracked {
            println!(
                "  {} {} repo(s), {}",
                out::node(&assessment.id),
                assessment.repo_count,
                out::muted(format!("{}, only untracked files", assessment.pr_basis()))
            );
        }
    }
    if !blocked_orphan_worktrees.is_empty() {
        println!(
            "{}",
            out::heading("Blocked orphan worktrees (use --force to prune):")
        );
        for orphan in &blocked_orphan_worktrees {
            println!(
                "  {} {}",
                out::node(&orphan.id),
                out::path(orphan.path.display())
            );
        }
    }
    if !orphan_worktrees.is_empty() {
        println!("{}", out::heading("Orphan worktree candidates:"));
        for orphan in &orphan_worktrees {
            if orphan.discards_pending {
                println!(
                    "  {} {} {}",
                    out::node(&orphan.id),
                    out::path(orphan.path.display()),
                    out::muted("discards uncommitted work")
                );
            } else {
                println!(
                    "  {} {}",
                    out::node(&orphan.id),
                    out::path(orphan.path.display())
                );
            }
        }
    }
    if !remote_orphans.is_empty() {
        println!("{}", out::heading("Remote orphan bundle candidates:"));
        for orphan in &remote_orphans {
            println!(
                "  {} {} ({})",
                out::node(&orphan.slug),
                out::muted("KnitHub record with no local bundle"),
                out::muted(orphan.reason)
            );
        }
    }

    if !apply {
        println!();
        println!(
            "{}",
            out::warn(format!(
                "Run `{}` to delete these bundle artifacts.",
                suggested_prune_apply_command(
                    untracked,
                    worktrees,
                    force || !blocked_orphan_worktrees.is_empty(),
                    branches,
                    force_branches,
                    remote_branches,
                    remote_bundles,
                )
            ))
        );
        return Ok(());
    }

    let mut pruned = 0usize;
    for candidate in candidates {
        delete_bundle(
            &candidate.id,
            true,
            worktrees,
            branches,
            force_branches,
            remote_branches,
            remote_bundles,
            config.as_ref(),
        )?;
        pruned += 1;
    }
    let mut removed_orphans = 0usize;
    for orphan in orphan_worktrees {
        remove_orphan_worktree(&orphan, force)?;
        removed_orphans += 1;
    }
    let mut removed_remote = 0usize;
    if let Some(config) = config.as_ref() {
        for orphan in remote_orphans {
            match crate::commands::remote::delete_remote_bundle_by_id(config, &orphan.remote_id) {
                Ok(slug) => {
                    println!(
                        "{}: {} {}",
                        out::node(&orphan.slug),
                        out::movement("deleted remote bundle"),
                        out::muted(slug)
                    );
                    removed_remote += 1;
                }
                Err(err) => print_prune_warning(format!(
                    "{}: failed to delete remote bundle record: {err:#}",
                    orphan.slug
                )),
            }
        }
    }

    println!("{} {} bundle(s)", out::heading("Pruned:"), pruned);
    if removed_orphans > 0 {
        println!(
            "{} {} orphan worktree dir(s)",
            out::heading("Removed:"),
            removed_orphans
        );
    }
    if removed_remote > 0 {
        println!(
            "{} {} remote orphan record(s)",
            out::heading("Removed:"),
            removed_remote
        );
    }
    Ok(())
}

/// Assess every bundle, returning the assessments plus the set of ids that exist
/// locally. Best-effort: an unreadable bundle file is skipped with a warning, and
/// a bundle that fails its scan is skipped rather than aborting the whole prune.
fn assess_bundles(
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
            Err(err) => {
                print_prune_warning(format!("{id}: skipped during prune scan: {err:#}"))
            }
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
        pending,
    })
}

struct RepoPruneSignals {
    publication_update: Option<PullRequest>,
    pending: Pending,
    pending_check_failed: bool,
    saw_publication: bool,
    saw_open_publication: bool,
    saw_merged_publication: bool,
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

    Ok(RepoPruneSignals {
        publication_update,
        pending,
        pending_check_failed,
        saw_publication,
        saw_open_publication,
        saw_merged_publication,
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

/// Find KnitHub remote bundle records that have no local artifact and whose recorded
/// PRs are all merged or closed. These can never be reached by the local `.knit/bundles/`
/// scan once their local artifact is gone, so prune would otherwise leave them forever.
fn remote_orphan_candidates(
    config: Option<&KnitConfig>,
    local_ids: &BTreeSet<String>,
    root: &Path,
    refresh: bool,
) -> Vec<RemoteOrphan> {
    let Some(config) = config else {
        return Vec::new();
    };
    let Some(project_id) = config.active_project.clone() else {
        print_prune_warning(
            "no active project configured; skipping KnitHub remote orphan detection",
        );
        return Vec::new();
    };
    let records = match crate::commands::remote::list_remote_bundles(config, &project_id) {
        Ok(records) => records,
        Err(err) => {
            print_prune_warning(format!(
                "could not list KnitHub remote bundles ({err:#}); skipping remote orphan detection"
            ));
            return Vec::new();
        }
    };
    let mut orphans = Vec::new();
    let cache = PruneCache::new();
    let jobs: Vec<RemoteOrphanJob> = records
        .into_iter()
        .filter_map(|record| {
            if record.lifecycle_state == "deleted" || local_ids.contains(&record.slug) {
                return None;
            }
            let payload = record.payload?;
            Some(RemoteOrphanJob {
                remote_id: record.remote_id,
                slug: record.slug,
                payload,
            })
        })
        .collect();

    let results: Vec<(String, Option<RemoteOrphan>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|job| {
                let slug = job.slug.clone();
                let remote_id = job.remote_id.clone();
                let payload = job.payload.clone();
                let cache = cache.clone();
                scope.spawn(move || {
                    (
                        slug.clone(),
                        remote_payload_dead_reason(root, &payload, refresh, &cache)
                            .map(|reason| RemoteOrphan {
                                remote_id,
                                slug,
                                reason,
                            }),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("remote orphan worker thread panicked"))
            .collect()
    });

    for (slug, orphan) in results {
        if let Some(orphan) = orphan {
            orphans.push(orphan);
        } else {
            let _ = slug;
        }
    }
    orphans
}

struct RemoteOrphanJob {
    remote_id: String,
    slug: String,
    payload: ChangeGroup,
}

/// Classify an orphaned remote bundle's pull requests. The remote's stored artifact can be
/// stale (it was pushed before the PR merged), so with `refresh` on we re-check each PR's
/// live state from its host by URL, falling back to the last synced state only when the
/// lookup fails. A bundle is dead only when it has publications and none are still open; one
/// with no recorded PRs is left alone in case it is unpublished work in progress.
fn remote_payload_dead_reason(
    root: &Path,
    payload: &ChangeGroup,
    refresh: bool,
    cache: &PruneCache,
) -> Option<&'static str> {
    if payload.publications.is_empty() {
        return None;
    }

    let states: Vec<String> = if refresh {
        std::thread::scope(|scope| {
            let handles: Vec<_> = payload
                .publications
                .iter()
                .map(|publication| {
                    let publication = publication.clone();
                    scope.spawn(move || {
                        refresh_remote_publication_state(root, &publication, cache)
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().expect("remote publication worker thread panicked"))
                .collect()
        })
    } else {
        payload
            .publications
            .iter()
            .map(|publication| publication.state.clone())
            .collect()
    };

    classify_remote_publication_states(&states)
}

fn refresh_remote_publication_state(
    root: &Path,
    publication: &crate::model::PublicationEntry,
    cache: &PruneCache,
) -> String {
    match providers::by_id(&publication.provider) {
        Some(forge) => match cache.view_pr(forge.as_ref(), root, &publication.url) {
            Ok(pr) => pr.state.unwrap_or_else(|| publication.state.clone()),
            Err(err) => {
                print_prune_warning(format!(
                    "could not refresh remote review object {} ({err:#}); using last synced state",
                    publication.url
                ));
                publication.state.clone()
            }
        },
        None => publication.state.clone(),
    }
}

fn classify_remote_publication_states(states: &[String]) -> Option<&'static str> {
    let mut saw_merged = false;
    for state in states {
        if publication_state_is_merged(state) {
            saw_merged = true;
        } else if !publication_state_is_closed(state) {
            return None;
        }
    }
    if saw_merged {
        Some("remote PRs are merged")
    } else {
        Some("remote PRs are closed")
    }
}

fn print_prune_report(assessments: &[PruneAssessment], untracked: bool) {
    println!("{}", out::heading("Bundle report:"));
    for assessment in assessments {
        let status = if let Some(reason) = assessment.candidate_reason(untracked) {
            format!("prunable — {reason}")
        } else if assessment.blocked_by_untracked_only() {
            "kept — only untracked files (prunable with --untracked)".to_string()
        } else if assessment.saw_open_publication {
            "kept — open PR(s)".to_string()
        } else if assessment.pending.tracked {
            "kept — uncommitted tracked changes".to_string()
        } else {
            "kept".to_string()
        };

        let mut detail = vec![
            format!("{} repo(s)", assessment.repo_count),
            assessment.pr_basis().to_string(),
        ];
        if assessment.pending.tracked {
            detail.push("tracked changes".to_string());
        }
        if assessment.pending.untracked {
            detail.push("untracked files".to_string());
        }

        println!(
            "  {} {}",
            out::node(&assessment.id),
            out::muted(status)
        );
        println!("      {}", out::muted(detail.join(", ")));
    }
    println!();
}

fn publication_state_is_merged(state: &str) -> bool {
    state.eq_ignore_ascii_case("merged")
}

fn publication_state_is_closed(state: &str) -> bool {
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

fn path_pending_changes(path: &Path) -> Result<Pending> {
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

fn orphan_worktree_candidates(
    root: &Path,
    force: bool,
) -> Result<(Vec<OrphanWorktree>, Vec<OrphanWorktree>)> {
    let worktrees_dir = root.join(".knit/worktrees");
    if !worktrees_dir.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut candidates = Vec::new();
    let mut blocked = Vec::new();
    for entry in fs::read_dir(&worktrees_dir)
        .with_context(|| format!("failed to read {}", worktrees_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if id.starts_with('.') || bundle_exists(root, &id) {
            continue;
        }
        let orphan = OrphanWorktree {
            id,
            path,
            discards_pending: false,
        };
        if path_pending_changes(&orphan.path)?.any() {
            if force {
                candidates.push(OrphanWorktree {
                    discards_pending: true,
                    ..orphan
                });
            } else {
                blocked.push(orphan);
            }
            continue;
        }
        candidates.push(orphan);
    }
    candidates.sort_by(|left, right| left.id.cmp(&right.id));
    blocked.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((candidates, blocked))
}

fn remove_orphan_worktree(orphan: &OrphanWorktree, force: bool) -> Result<()> {
    let worktrees = git_worktrees_under(&orphan.path)?;
    for worktree in worktrees {
        if is_linked_worktree(&worktree) {
            remove_git_worktree_from_self(&worktree, force)?;
        }
    }
    if orphan.path.exists() {
        fs::remove_dir_all(&orphan.path)
            .with_context(|| format!("failed to remove {}", orphan.path.display()))?;
    }
    println!(
        "{}: {} {}",
        out::node(&orphan.id),
        out::movement("removed orphan worktree"),
        out::path(orphan.path.display())
    );
    Ok(())
}

fn is_linked_worktree(path: &Path) -> bool {
    path.join(".git").is_file()
}

fn git_worktrees_under(path: &Path) -> Result<Vec<PathBuf>> {
    let mut worktrees = Vec::new();
    collect_git_worktrees(path, &mut worktrees)?;
    worktrees.sort();
    Ok(worktrees)
}

fn collect_git_worktrees(path: &Path, worktrees: &mut Vec<PathBuf>) -> Result<()> {
    if !path.is_dir() {
        return Ok(());
    }
    if is_git_worktree(path) {
        worktrees.push(path.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        collect_git_worktrees(&entry?.path(), worktrees)?;
    }
    Ok(())
}

fn remove_git_worktree_from_self(worktree: &Path, force: bool) -> Result<()> {
    let mut args = vec![OsString::from("worktree"), OsString::from("remove")];
    if force {
        args.push(OsString::from("--force"));
    }
    args.push(worktree.as_os_str().to_os_string());
    git_output(worktree, args)?;
    Ok(())
}

fn suggested_prune_apply_command(
    untracked: bool,
    worktrees: bool,
    force: bool,
    branches: bool,
    force_branches: bool,
    remote_branches: bool,
    remote_bundles: bool,
) -> String {
    if worktrees && force && branches && force_branches && remote_branches && remote_bundles
    {
        let base = "knit bundle prune --apply --all";
        return if untracked {
            format!("{base} --untracked")
        } else {
            base.to_string()
        };
    }
    let mut command = vec!["knit", "bundle", "prune", "--apply"];
    if untracked {
        command.push("--untracked");
    }
    if worktrees {
        command.push("--worktrees");
    }
    if force {
        command.push("--force");
    }
    if branches {
        command.push("--branches");
    }
    if force_branches {
        command.push("--force-branches");
    }
    if remote_branches {
        command.push("--remote-branches");
    }
    if remote_bundles {
        command.push("--remote-bundles");
    }
    command.join(" ")
}

#[cfg(test)]
mod prune_tests {
    use super::*;
    use crate::model::PublicationEntry;

    fn bundle_with_publication_states(states: &[&str]) -> ChangeGroup {
        let mut bundle = ChangeGroup::new(
            "feature".to_string(),
            "feature".to_string(),
            "2026-05-05T00:00:00.000Z".to_string(),
        );
        bundle.publications = states
            .iter()
            .enumerate()
            .map(|(index, state)| PublicationEntry {
                repo_id: format!("repo{index}"),
                provider: "github".to_string(),
                kind: providers::PULL_REQUEST_KIND.to_string(),
                number: index as u64 + 1,
                url: format!("https://github.com/acme/repo{index}/pull/{}", index + 1),
                base_branch: "main".to_string(),
                head_branch: format!("knit/feature-{index}"),
                state: (*state).to_string(),
                title: None,
                updated_at: "2026-05-05T00:00:00.000Z".to_string(),
            })
            .collect();
        bundle
    }

    // refresh=false keeps these pure (no host lookups), exercising the classification of
    // an orphan remote bundle from its recorded publication states.
    fn dead_reason(states: &[&str]) -> Option<&'static str> {
        let cache = PruneCache::new();
        remote_payload_dead_reason(
            Path::new("/nonexistent"),
            &bundle_with_publication_states(states),
            false,
            &cache,
        )
    }

    #[test]
    fn all_merged_publications_are_dead() {
        assert_eq!(dead_reason(&["MERGED", "merged"]), Some("remote PRs are merged"));
    }

    #[test]
    fn all_closed_publications_are_dead() {
        assert_eq!(dead_reason(&["CLOSED"]), Some("remote PRs are closed"));
    }

    #[test]
    fn merged_and_closed_without_open_is_dead() {
        assert_eq!(dead_reason(&["MERGED", "CLOSED"]), Some("remote PRs are merged"));
    }

    #[test]
    fn any_open_publication_keeps_the_bundle() {
        assert_eq!(dead_reason(&["MERGED", "OPEN"]), None);
    }

    #[test]
    fn no_publications_is_not_an_orphan() {
        assert_eq!(dead_reason(&[]), None);
    }

    #[test]
    fn suggested_command_includes_force_when_needed() {
        let cmd = suggested_prune_apply_command(false, true, true, false, false, false, false);
        assert_eq!(cmd, "knit bundle prune --apply --worktrees --force");
    }
}
