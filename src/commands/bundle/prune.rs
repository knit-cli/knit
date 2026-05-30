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
use crate::store::{bundle_exists, load_config, read_json, write_json};
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

struct PruneCache {
    pr_by_url: Arc<Mutex<HashMap<String, PullRequest>>>,
    pr_by_branch: Arc<Mutex<HashMap<BranchKey, Option<PullRequest>>>>,
    pending_changes: Arc<Mutex<HashMap<String, Pending>>>,
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
        }
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
        Some(load_config(&root)?)
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

    let orphan_worktrees = if worktrees {
        orphan_worktree_candidates(&root)?
    } else {
        Vec::new()
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
    if !orphan_worktrees.is_empty() {
        println!("{}", out::heading("Orphan worktree candidates:"));
        for orphan in &orphan_worktrees {
            println!(
                "  {} {}",
                out::node(&orphan.id),
                out::path(orphan.path.display())
            );
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
        remove_orphan_worktree(&orphan, force_branches)?;
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

    let results: Vec<(String, Result<PruneAssessment>)> = if refresh {
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (path, mut bundle) in bundles {
                handles.push(scope.spawn(move || {
                    let id = bundle.id.clone();
                    (id, assess_bundle(root, &path, &mut bundle, true, cache))
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        })
    } else {
        bundles
            .into_iter()
            .map(|(path, mut bundle)| {
                let id = bundle.id.clone();
                (id, assess_bundle(root, &path, &mut bundle, false, cache))
            })
            .collect()
    };

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
    let repos = bundle.repos.clone();
    let mut saw_publication = false;
    let mut saw_merged_publication = false;
    let mut saw_open_publication = false;
    let mut pending = Pending::default();
    for repo in repos {
        let branch = repo.feature_branch.as_deref();
        let mut publication = providers::publication_for_repo(bundle, &repo.id).cloned();

        // Skip review-state refresh for repos whose host is not recognized;
        // prune still proceeds on the remaining signals.
        // A failed lookup or checkout probe must not abort the whole prune: warn
        // and fall back to the last recorded state, keeping the bundle when the
        // checkout is unverifiable.
        if refresh {
            if let Ok(forge) = providers::for_repo(&repo) {
                if let Some(existing) = publication.as_ref() {
                    match cache.view_pr(forge.as_ref(), Path::new(&repo.path), &existing.url) {
                        Ok(pr) => {
                            providers::upsert_publication(bundle, &repo, forge.as_ref(), &pr);
                            publication = providers::publication_for_repo(bundle, &repo.id).cloned();
                        }
                        Err(err) => print_prune_warning(format!(
                            "{}/{}: could not refresh {} ({err:#}); using last recorded state",
                            bundle.id, repo.id, existing.url
                        )),
                    }
                } else if let Some(branch) = branch {
                    match cache.find_existing_pr(
                        forge.as_ref(),
                        Path::new(&repo.path),
                        branch,
                        &repo.base_branch,
                    ) {
                        Ok(Some(pr)) => {
                            providers::upsert_publication(bundle, &repo, forge.as_ref(), &pr);
                            publication = providers::publication_for_repo(bundle, &repo.id).cloned();
                        }
                        Ok(None) => {}
                        Err(err) => print_prune_warning(format!(
                            "{}/{}: could not check for an open review object on {branch} ({err:#}); using last recorded state",
                            bundle.id, repo.id
                        )),
                    }
                }
            }
        }

        match cache.checkout_has_pending_changes(root, &repo) {
            Ok(found) => pending.merge(found),
            Err(err) => {
                print_prune_warning(format!(
                    "{}/{}: could not inspect checkout for pending changes ({err:#}); keeping the bundle to be safe",
                    bundle.id, repo.id
                ));
                pending.tracked = true;
            }
        }

        let Some(publication) = publication else {
            continue;
        };
        saw_publication = true;
        if Some(publication.head_branch.as_str()) != branch {
            saw_open_publication = true;
            continue;
        }
        if publication_state_is_merged(&publication.state) {
            saw_merged_publication = true;
        } else if !publication_state_is_closed(&publication.state) {
            saw_open_publication = true;
        }
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
    for record in records {
        if record.lifecycle_state == "deleted" || local_ids.contains(&record.slug) {
            continue;
        }
        let Some(payload) = record.payload.as_ref() else {
            continue;
        };
        if let Some(reason) = remote_payload_dead_reason(root, payload, refresh) {
            orphans.push(RemoteOrphan {
                remote_id: record.remote_id,
                slug: record.slug,
                reason,
            });
        }
    }
    orphans
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
) -> Option<&'static str> {
    let mut saw_publication = false;
    let mut saw_merged = false;
    for publication in &payload.publications {
        saw_publication = true;
        let state = if refresh {
            match providers::by_id(&publication.provider) {
                Some(forge) => match forge.view(&PrTarget::checkout(root), &publication.url) {
                    Ok(pr) => pr.state.unwrap_or_else(|| publication.state.clone()),
                    Err(err) => {
                        print_prune_warning(format!(
                            "{}: could not refresh remote review object {} ({err:#}); using last synced state",
                            payload.id, publication.url
                        ));
                        publication.state.clone()
                    }
                },
                None => publication.state.clone(),
            }
        } else {
            publication.state.clone()
        };
        if publication_state_is_merged(&state) {
            saw_merged = true;
        } else if !publication_state_is_closed(&state) {
            return None;
        }
    }
    if !saw_publication {
        return None;
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

fn orphan_worktree_candidates(root: &Path) -> Result<Vec<OrphanWorktree>> {
    let worktrees_dir = root.join(".knit/worktrees");
    if !worktrees_dir.exists() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
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
        if path_pending_changes(&path)?.any() {
            println!(
                "{}: {} {}",
                out::node(&id),
                out::muted("orphan worktree has pending files, preserved"),
                out::path(path.display())
            );
            continue;
        }
        candidates.push(OrphanWorktree { id, path });
    }
    candidates.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(candidates)
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
    branches: bool,
    force_branches: bool,
    remote_branches: bool,
    remote_bundles: bool,
) -> String {
    if worktrees && branches && force_branches && remote_branches && remote_bundles {
        let base = "knit prune --apply --all";
        return if untracked {
            format!("{base} --untracked")
        } else {
            base.to_string()
        };
    }
    let mut command = vec!["knit", "prune", "--apply"];
    if untracked {
        command.push("--untracked");
    }
    if worktrees {
        command.push("--worktrees");
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
        remote_payload_dead_reason(
            Path::new("/nonexistent"),
            &bundle_with_publication_states(states),
            false,
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
}
