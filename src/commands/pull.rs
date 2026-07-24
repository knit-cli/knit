use crate::checkout::checkout_dir;
use crate::commands::base::advance_local_base;
use crate::commands::bundle::list_open_bundle_ids;
use crate::commands::project::load_project_by_id;
use crate::commands::remote::{
    prepare_remote_pull, pull_bundle_remote_state, pull_remote_state, RemoteBundleOutcome,
    RemotePullContext,
};
use crate::git::{current_branch, git_output, rev_parse};
use crate::ids::short_sha;
use crate::model::{ChangeGroup, ProjectRepoEntry, RepoEntry};
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::status::status_label;
use crate::store::{
    bundle_path, find_knit_root, load_active_bundle, load_active_bundle_for_update, load_config,
    read_json, save_active_bundle, ActiveBundle, BundleResolutionSource,
};
use crate::time::now_iso;
use crate::tracking::{sync_note, sync_observed_changes_for_repo_ids};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub fn pull_repos(
    selectors: &[String],
    all: bool,
    rebase: bool,
    force: bool,
    feature: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let targets = indexes
        .iter()
        .map(|index| {
            let repo = &active.bundle.repos[*index];
            let cwd = pull_cwd(&active, repo, feature)?;
            Ok(PullTarget {
                repo_index: *index,
                repo_id: repo.id.clone(),
                cwd,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    preflight_clean(&targets, force)?;

    let results: Vec<(String, Result<PullOutcome>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|target| {
                let repo_id = target.repo_id.clone();
                scope.spawn(move || (repo_id, run_pull_target(target, rebase)))
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("pull worker thread panicked"))
            .collect()
    });

    let mut bundle_changed = false;
    let mut failures = Vec::new();
    for (repo_id, result) in results {
        match result {
            Ok(outcome) => {
                print_pull_summary(&repo_id, &outcome.before, &outcome.after);
                if !feature {
                    let repo = &mut active.bundle.repos[outcome.repo_index];
                    if repo.base_sha.as_deref() != Some(outcome.after.as_str()) {
                        repo.base_sha = Some(outcome.after);
                        bundle_changed = true;
                    }
                }
            }
            Err(error) => {
                println!("{}: {}", out::repo(&repo_id), out::danger("pull failed"));
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    if !failures.is_empty() {
        bail!("pull failed:\n{}", failures.join("\n"));
    }

    if feature {
        let repo_ids = targets
            .iter()
            .map(|target| target.repo_id.clone())
            .collect::<Vec<_>>();
        let changes = sync_observed_changes_for_repo_ids(&mut active, Some(&repo_ids))?;
        for change in &changes {
            println!("{}: {}", out::repo(&change.repo_id), sync_note(change));
        }
        bundle_changed = !changes.is_empty();
    }

    if bundle_changed {
        active.bundle.updated_at = now_iso();
        save_active_bundle(&active)?;
    }

    Ok(())
}

struct PullTarget {
    repo_index: usize,
    repo_id: String,
    cwd: PathBuf,
}

struct PullOutcome {
    repo_index: usize,
    before: String,
    after: String,
}

fn run_pull_target(target: &PullTarget, rebase: bool) -> Result<PullOutcome> {
    let before = rev_parse(&target.cwd, "HEAD")
        .with_context(|| format!("{}: failed to read HEAD before pull", target.repo_id))?;
    run_pull(&target.cwd, rebase)
        .with_context(|| format!("{}: git pull failed", target.repo_id))?;
    let after = rev_parse(&target.cwd, "HEAD")
        .with_context(|| format!("{}: failed to read HEAD after pull", target.repo_id))?;
    Ok(PullOutcome {
        repo_index: target.repo_index,
        before,
        after,
    })
}

fn pull_cwd(active: &ActiveBundle, repo: &RepoEntry, feature: bool) -> Result<PathBuf> {
    if feature {
        let Some(cwd) = checkout_dir(active, repo) else {
            bail!("{}: no feature checkout is recorded.", repo.id);
        };
        ensure_feature_branch(repo, &cwd)?;
        return Ok(cwd);
    }

    let cwd = PathBuf::from(&repo.path);
    if !cwd.exists() {
        bail!(
            "{}: original repo path does not exist: {}",
            repo.id,
            cwd.display()
        );
    }

    ensure_base_branch(repo, &cwd)?;
    Ok(cwd)
}

fn ensure_feature_branch(repo: &RepoEntry, cwd: &Path) -> Result<()> {
    let expected = repo
        .feature_branch
        .as_deref()
        .with_context(|| format!("{}: no feature branch is recorded.", repo.id))?;
    let actual = current_branch(cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != expected {
        bail!(
            "{}: feature pull expected branch `{expected}`, found `{actual}` in {}.",
            repo.id,
            cwd.display()
        );
    }

    Ok(())
}

fn ensure_base_branch(repo: &RepoEntry, cwd: &Path) -> Result<()> {
    let actual = current_branch(cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual == repo.feature_branch.as_deref().unwrap_or("") {
        bail!(
            "{}: default pull does not operate on the feature branch. Use `knit pull --feature {}` if that is intentional.",
            repo.id,
            repo.id
        );
    }
    if actual != repo.base_branch {
        bail!(
            "{}: default pull expected base branch `{}`, found `{actual}` in {}. Checkout the base branch or use `knit pull --feature {}`.",
            repo.id,
            repo.base_branch,
            cwd.display(),
            repo.id
        );
    }

    Ok(())
}

fn preflight_clean(targets: &[PullTarget], force: bool) -> Result<()> {
    if force {
        return Ok(());
    }

    let mut dirty = Vec::new();
    for target in targets {
        let status = git_output(&target.cwd, ["status", "--short"])
            .with_context(|| format!("{}: failed to inspect status", target.repo_id))?;
        if !status.trim().is_empty() {
            dirty.push(format!(
                "{}: {} in {}",
                target.repo_id,
                status_label(&status),
                target.cwd.display()
            ));
        }
    }

    if !dirty.is_empty() {
        bail!(
            "Refusing to pull with uncommitted changes. Commit, stash, or pass --force to let git decide:\n{}",
            dirty.join("\n")
        );
    }

    Ok(())
}

fn run_pull(cwd: &Path, rebase: bool) -> Result<()> {
    let mut args = vec![OsString::from("pull")];
    if rebase {
        args.push(OsString::from("--rebase"));
    } else {
        args.push(OsString::from("--ff-only"));
    }

    git_output(cwd, args)?;
    Ok(())
}

fn print_pull_summary(repo_id: &str, before: &str, after: &str) {
    if before == after {
        println!(
            "{}: {} {}",
            out::repo(repo_id),
            out::muted("unchanged"),
            out::sha(short_sha(after))
        );
    } else {
        println!(
            "{}: {} {} -> {}",
            out::repo(repo_id),
            out::movement("advanced"),
            out::sha(short_sha(before)),
            out::sha(short_sha(after))
        );
    }
}

// ---------------------------------------------------------------------------
// Context-aware pull orchestrator
// ---------------------------------------------------------------------------

/// Entry point for `knit pull`. Decides what to update from context and flags:
/// - `--base`, `--current`, or `--bundles` run the aggregate report.
/// - With no target flags: inside a resolved bundle, pull that bundle's recorded
///   base source checkouts plus its remote artifact; at the workspace base (the
///   shared fallback) pull current source checkouts plus every open bundle.
#[allow(clippy::too_many_arguments)]
pub fn pull(
    selectors: &[String],
    all: bool,
    rebase: bool,
    force: bool,
    feature: bool,
    main: bool,
    base: bool,
    current: bool,
    bundles: bool,
    remote: Option<&str>,
    no_remote: bool,
    merge: bool,
) -> Result<()> {
    let current = current || main;
    if base || current || bundles {
        return aggregate_pull(
            base, current, bundles, rebase, force, remote, no_remote, merge,
        );
    }

    // A bare `knit pull` (no repo/flag target) at the workspace base means
    // "update everything": the project's source checkouts plus every open bundle.
    // Explicit selectors, `--all`, or `--feature` keep the single-bundle meaning.
    if selectors.is_empty() && !all && !feature {
        let active = load_active_bundle()?;
        if active.resolution_source == BundleResolutionSource::Config {
            return aggregate_pull(false, true, true, rebase, force, remote, no_remote, merge);
        }
    }

    // A specific bundle is in context: pull its repos (and its remote artifact).
    pull_repos(selectors, all, rebase, force, feature)?;
    pull_remote_state(remote, no_remote, merge)
}

/// Result of pulling one unit (a project repo's source checkout, or a bundle).
enum Outcome {
    Advanced { before: String, after: String },
    Unchanged(String),
    Synced(String),
    Refreshed(String),
    Skipped(String),
    Failed(String),
}

/// Serializes git work that touches the same source repo while letting distinct
/// repos run concurrently. Base/current work and any bundle that includes the
/// same repo share one lock; unrelated repos never block each other.
#[derive(Default)]
struct RepoGate {
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl RepoGate {
    fn mutex_for(&self, path: &Path) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Run `op` while holding the lock for every given repo path. Paths are
    /// acquired in a globally consistent (sorted) order, so concurrent callers
    /// holding overlapping sets cannot deadlock.
    fn lock_all<R>(&self, paths: &[PathBuf], op: impl FnOnce() -> R) -> R {
        let mut ordered: Vec<PathBuf> = paths.to_vec();
        ordered.sort();
        ordered.dedup();
        let arcs: Vec<Arc<Mutex<()>>> = ordered.iter().map(|path| self.mutex_for(path)).collect();
        let _guards: Vec<_> = arcs.iter().map(|arc| arc.lock().unwrap()).collect();
        op()
    }
}

/// Update project bases, current checkouts, and/or every open bundle in parallel,
/// reporting each target's outcome instead of aborting on the first problem.
#[allow(clippy::too_many_arguments)]
fn aggregate_pull(
    base: bool,
    current: bool,
    bundles: bool,
    rebase: bool,
    force: bool,
    remote: Option<&str>,
    no_remote: bool,
    merge: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;

    let project_repos = match ((base || current), config.active_project.clone()) {
        (true, Some(project_id)) => load_project_by_id(&root, &project_id)?.repos,
        // No active project: project-wide targets have nothing to update, but
        // the run should still process any requested bundles.
        _ => Vec::new(),
    };
    let base_repos: Vec<ProjectRepoEntry> = if base {
        project_repos.clone()
    } else {
        Vec::new()
    };
    let current_repos: Vec<(String, PathBuf)> = if current {
        project_repos
            .iter()
            .map(|repo| (repo.id.clone(), PathBuf::from(&repo.path)))
            .collect()
    } else {
        Vec::new()
    };

    let bundle_ids = if bundles {
        list_open_bundle_ids(&root)?
    } else {
        Vec::new()
    };
    let remote_context = if bundles {
        prepare_remote_pull(remote, no_remote)?
    } else {
        None
    };
    let bundle_repo_paths: HashMap<String, Vec<PathBuf>> = bundle_ids
        .iter()
        .map(|id| (id.clone(), read_bundle_repo_paths(&root, id)))
        .collect();

    let gate = RepoGate::default();
    let mut base_results: Vec<(String, Outcome)> = Vec::new();
    let mut current_results: Vec<(String, Outcome)> = Vec::new();
    let mut bundle_results: Vec<(String, Outcome)> = Vec::new();

    std::thread::scope(|scope| {
        let base_handles: Vec<_> = base_repos
            .iter()
            .map(|repo| {
                let gate = &gate;
                scope.spawn(move || {
                    let path = PathBuf::from(&repo.path);
                    let outcome = gate.lock_all(std::slice::from_ref(&path), || pull_base(repo));
                    (repo.id.clone(), outcome)
                })
            })
            .collect();

        let current_handles: Vec<_> = current_repos
            .iter()
            .map(|(id, path)| {
                let gate = &gate;
                scope.spawn(move || {
                    let outcome = gate.lock_all(std::slice::from_ref(path), || {
                        pull_path(path, rebase, force)
                    });
                    (id.clone(), outcome)
                })
            })
            .collect();

        let bundle_handles: Vec<_> = bundle_ids
            .iter()
            .map(|id| {
                let gate = &gate;
                let context = remote_context.as_ref();
                let root = root.as_path();
                let paths = bundle_repo_paths.get(id).cloned().unwrap_or_default();
                // The workspace's pointed-at bundle gets the deep refresh:
                // missing worktrees are materialized so `knit fetch` +
                // `knit switch` + `knit pull` yields a usable checkout. Other
                // open bundles only refresh checkouts they already have.
                let materialize = config.active_bundle.as_deref() == Some(id.as_str());
                scope.spawn(move || {
                    let outcome = gate.lock_all(&paths, || {
                        pull_one_bundle(root, context, id, merge, materialize)
                    });
                    (id.clone(), outcome)
                })
            })
            .collect();

        base_results = base_handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        current_results = current_handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        bundle_results = bundle_handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
    });

    report(&base_results, &current_results, &bundle_results);
    if base_results
        .iter()
        .any(|(_, outcome)| matches!(outcome, Outcome::Failed(_)))
    {
        bail!("one or more configured base branches could not be updated");
    }
    Ok(())
}

fn pull_base(repo: &ProjectRepoEntry) -> Outcome {
    let path = PathBuf::from(&repo.path);
    match advance_local_base(&path, &repo.base_branch) {
        Ok(update) => match update.before {
            Some(before) if before == update.after => Outcome::Unchanged(update.after),
            Some(before) => Outcome::Advanced {
                before,
                after: update.after,
            },
            None => Outcome::Advanced {
                before: "(missing)".to_string(),
                after: update.after,
            },
        },
        Err(error) => Outcome::Failed(condense(&error)),
    }
}

/// Best-effort `git pull` of one checkout on its current branch. Never bails:
/// a dirty tree, a non-fast-forward, or a git error becomes a reported outcome.
fn pull_path(cwd: &Path, rebase: bool, force: bool) -> Outcome {
    if !cwd.exists() {
        return Outcome::Failed("source path does not exist".to_string());
    }
    if !force {
        match git_output(cwd, ["status", "--short"]) {
            Ok(status) if !status.trim().is_empty() => {
                return Outcome::Skipped("uncommitted changes".to_string())
            }
            Ok(_) => {}
            Err(error) => return Outcome::Failed(condense(&error)),
        }
    }
    let before = match rev_parse(cwd, "HEAD") {
        Ok(sha) => sha,
        Err(error) => return Outcome::Failed(condense(&error)),
    };
    let mut args = vec![OsString::from("pull")];
    args.push(OsString::from(if rebase {
        "--rebase"
    } else {
        "--ff-only"
    }));
    if let Err(error) = git_output(cwd, args) {
        return Outcome::Failed(condense(&error));
    }
    let after = match rev_parse(cwd, "HEAD") {
        Ok(sha) => sha,
        Err(error) => return Outcome::Failed(condense(&error)),
    };
    if before == after {
        Outcome::Unchanged(after)
    } else {
        Outcome::Advanced { before, after }
    }
}

fn pull_one_bundle(
    root: &Path,
    context: Option<&RemotePullContext>,
    bundle_id: &str,
    merge: bool,
    materialize: bool,
) -> Outcome {
    let Some(context) = context else {
        return Outcome::Skipped("no sync remote available".to_string());
    };
    match pull_bundle_remote_state(root, context, bundle_id, merge, materialize) {
        Ok(RemoteBundleOutcome::Pulled(hash)) => Outcome::Synced(hash),
        Ok(RemoteBundleOutcome::Merged(hash)) => {
            Outcome::Synced(format!("{hash} (merged ledgers)"))
        }
        Ok(RemoteBundleOutcome::Refreshed(summary)) => Outcome::Refreshed(summary),
        Ok(RemoteBundleOutcome::Skipped(reason)) => Outcome::Skipped(reason),
        Err(error) => Outcome::Failed(condense(&error)),
    }
}

fn read_bundle_repo_paths(root: &Path, bundle_id: &str) -> Vec<PathBuf> {
    let path = bundle_path(root, bundle_id);
    match read_json::<ChangeGroup>(&path) {
        Ok(bundle) => bundle
            .repos
            .iter()
            .map(|repo| PathBuf::from(&repo.path))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Collapse a multi-line git/anyhow error into a single reportable line,
/// dropping git's advice ("hint:") noise.
fn condense(error: &anyhow::Error) -> String {
    format!("{error:#}")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("hint:"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn report(
    base_results: &[(String, Outcome)],
    current_results: &[(String, Outcome)],
    bundle_results: &[(String, Outcome)],
) {
    let mut totals = Totals::default();
    if !base_results.is_empty() {
        println!("{}", out::heading("Base branches:"));
        for (id, outcome) in base_results {
            print_outcome(id, outcome);
            totals.add(outcome);
        }
    }
    if !current_results.is_empty() {
        println!("{}", out::heading("Current checkouts:"));
        for (id, outcome) in current_results {
            print_outcome(id, outcome);
            totals.add(outcome);
        }
    }
    if !bundle_results.is_empty() {
        println!("{}", out::heading("Bundles:"));
        for (id, outcome) in bundle_results {
            print_outcome(id, outcome);
            totals.add(outcome);
        }
    }
    if base_results.is_empty() && current_results.is_empty() && bundle_results.is_empty() {
        println!("{}", out::muted("Nothing to pull."));
        return;
    }
    println!(
        "{} {} advanced, {} unchanged, {} synced, {} refreshed, {} skipped, {} failed",
        out::heading("Pulled:"),
        totals.advanced,
        totals.unchanged,
        totals.synced,
        totals.refreshed,
        totals.skipped,
        totals.failed
    );
}

#[derive(Default)]
struct Totals {
    advanced: usize,
    unchanged: usize,
    synced: usize,
    refreshed: usize,
    skipped: usize,
    failed: usize,
}

impl Totals {
    fn add(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Advanced { .. } => self.advanced += 1,
            Outcome::Unchanged(_) => self.unchanged += 1,
            Outcome::Synced(_) => self.synced += 1,
            Outcome::Refreshed(_) => self.refreshed += 1,
            Outcome::Skipped(_) => self.skipped += 1,
            Outcome::Failed(_) => self.failed += 1,
        }
    }
}

fn print_outcome(id: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Advanced { before, after } => println!(
            "  {} {} {} -> {}",
            out::repo(id),
            out::movement("advanced"),
            out::sha(short_sha(before)),
            out::sha(short_sha(after))
        ),
        Outcome::Unchanged(sha) => println!(
            "  {} {} {}",
            out::repo(id),
            out::muted("unchanged"),
            out::sha(short_sha(sha))
        ),
        Outcome::Synced(hash) => println!(
            "  {} {} {}",
            out::repo(id),
            out::movement("pulled"),
            out::muted(hash)
        ),
        Outcome::Refreshed(summary) => println!(
            "  {} {} {}",
            out::repo(id),
            out::movement("refreshed"),
            out::muted(summary)
        ),
        Outcome::Skipped(reason) => println!(
            "  {} {} ({})",
            out::repo(id),
            out::muted("skipped"),
            out::muted(reason)
        ),
        Outcome::Failed(reason) => println!(
            "  {} {} ({})",
            out::repo(id),
            out::danger("failed"),
            out::muted(reason)
        ),
    }
}
