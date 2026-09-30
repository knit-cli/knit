use crate::checkout::checkout_dir;
use crate::git::{
    current_branch, git_output, git_output_optional, git_output_with_timeout, ref_commit_sha,
    remote_ref_sha, rev_parse,
};
use crate::ids::short_sha;
use crate::model::{BundleState, ChangeGroup, RepoEntry};
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use crate::tracking::latest_recorded_head_sha;
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

struct PushSuccess {
    upstream: String,
    sha: String,
}

/// What a push worker reports. `Note` carries mid-push lines (a retry, say)
/// so they are printed in the main thread's stream rather than raced onto
/// stdout from a worker.
enum PushEvent {
    Note(String),
    Done {
        repo_id: String,
        result: Result<PushSuccess>,
    },
}

/// How `git push` may move the remote branch. Mirrors git's own flags:
/// `WithLease` pins an accepted remote tip before the first push attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushForce {
    No,
    WithLease,
    Unconditional,
}

impl PushForce {
    pub fn from_flags(force_with_lease: bool, force: bool) -> Self {
        match (force_with_lease, force) {
            (true, _) => Self::WithLease,
            (_, true) => Self::Unconditional,
            _ => Self::No,
        }
    }

    fn git_arg(self) -> Option<&'static str> {
        match self {
            Self::No => None,
            Self::WithLease => None, // Resolved explicitly before the retry loop.
            Self::Unconditional => Some("--force"),
        }
    }

    /// Whether this mode forces at all. Shared by the git plane and the
    /// bundle-artifact plane: the same flag pair covers both, so one
    /// `knit push --force-with-lease` moves rewritten branches and the
    /// rewritten ledger together.
    pub fn is_force(self) -> bool {
        !matches!(self, Self::No)
    }

    /// Whether the force is guarded by a lease: the overwrite must only be
    /// accepted if the remote still holds the state this client last saw.
    pub fn wants_lease(self) -> bool {
        matches!(self, Self::WithLease)
    }
}

fn ensure_no_pending_rewrite(root: &Path, bundle: &ChangeGroup) -> Result<()> {
    if root
        .join(".knit/rebase")
        .join(format!("{}.json", bundle.id))
        .exists()
    {
        bail!("A bundle rewrite is pending. Run knit rebase --continue or knit rebase --abort before pushing.");
    }
    Ok(())
}

pub fn push_repos(
    selectors: &[String],
    all: bool,
    set_upstream: bool,
    force: PushForce,
    remote: &[String],
    no_remote: bool,
    no_history: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    ensure_no_pending_rewrite(&active.root, &active.bundle)?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let mut discovered = false;
    for &index in &indexes {
        if let Some(cwd) = checkout_dir(&active, &active.bundle.repos[index]) {
            discovered |= crate::contribution::discover(&cwd, &mut active.bundle.repos[index])?;
        }
    }
    crate::contribution::validate_bundle(&active.bundle)?;
    for &index in &indexes {
        let repo = &active.bundle.repos[index];
        if crate::contribution::configured(repo) {
            if force == PushForce::Unconditional && crate::contribution::cross_repository(repo)? {
                bail!("force push is not supported for cross-repository contributions");
            }
            let cwd = checkout_dir(&active, repo).context("missing contribution checkout")?;
            crate::contribution::push_remote(&cwd, repo)?;
        }
    }
    if discovered {
        save_active_bundle(&active)?;
    }
    let total = indexes.len();
    let limit = crate::parallel::git_jobs()?;
    if total > 1 {
        let bound = if total > limit {
            format!(", {limit} at a time")
        } else {
            String::new()
        };
        println!("{}", out::muted(format!("pushing {total} repo(s){bound}…")));
    }

    // Report each repo the moment its push finishes rather than after the
    // slowest one: with many repos and a slow origin, a report batched after
    // the last join reads as a hang. The pool is bounded so a hundred-repo
    // bundle does not open a hundred connections at once.
    let (tx, rx) = std::sync::mpsc::channel();
    let failures: Vec<String> = std::thread::scope(|scope| {
        let active_ref = &active;
        let sender = tx.clone();
        crate::parallel::spawn_bounded(scope, &indexes, limit, move |&index| {
            let repo = &active_ref.bundle.repos[index];
            let repo_id = repo.id.clone();
            // Retry notes travel the same channel as the result, so the main
            // thread stays the only writer and lines never interleave.
            let notes = sender.clone();
            let note_repo = repo_id.clone();
            let _notes = crate::retry::stream_notes_to(move |line| {
                let _ = notes.send(PushEvent::Note(format!(
                    "{}: {line}",
                    out::repo(&note_repo)
                )));
            });
            let result = push_repo(active_ref, repo, set_upstream, force);
            // The receiver outlives every worker; a send cannot fail.
            let _ = sender.send(PushEvent::Done { repo_id, result });
        });
        drop(tx);

        let mut failures = Vec::new();
        let mut done = 0;
        for event in rx {
            match event {
                PushEvent::Note(line) => println!("{line}"),
                PushEvent::Done { repo_id, result } => {
                    done += 1;
                    let progress = out::progress(done, total);
                    match result {
                        Ok(success) => {
                            println!(
                                "{}: {} {} {}{progress}",
                                out::repo(&repo_id),
                                out::movement("pushed"),
                                out::branch(success.upstream),
                                out::sha(short_sha(&success.sha))
                            );
                        }
                        Err(error) => {
                            println!(
                                "{}: {}{progress}",
                                out::repo(&repo_id),
                                out::danger("push failed")
                            );
                            failures.push(format!("{repo_id}: {error:#}"));
                        }
                    }
                }
            }
        }
        failures
    });

    if !failures.is_empty() {
        let advice = if force.wants_lease() {
            "Fetch and inspect the remote branch before retrying a rejected lease; integrate any unrecorded work before forcing."
        } else {
            "re-run the same `knit push` to retry only these repos; branches already on origin are up to date."
        };
        bail!("push failed:\n{}\n\n{advice}", failures.join("\n"));
    }

    // A successful push may publish commits authored or rewritten outside
    // Knit. Record those heads before uploading the bundle artifact, so its
    // ledger and contribution identity describe the branches just published.
    let repo_ids: Vec<String> = indexes
        .iter()
        .map(|&index| active.bundle.repos[index].id.clone())
        .collect();
    let changes =
        crate::tracking::sync_observed_changes_for_repo_ids(&mut active, Some(&repo_ids))?;
    if !changes.is_empty() {
        save_active_bundle(&active)?;
    }

    // After git branches are pushed, also sync the bundle artifact to the
    // configured sync remote (default on; see `knit config set push-sync`).
    // The force mode carries over: a forced branch push implies the ledger
    // rewrite must be forced onto the sync remote too.
    crate::commands::remote::maybe_sync_bundle_to_remote_with_history(
        &mut active,
        remote,
        no_remote,
        force,
        !no_history,
    )?;

    Ok(())
}

fn push_repo(
    active: &ActiveBundle,
    repo: &RepoEntry,
    set_upstream: bool,
    force: PushForce,
) -> Result<PushSuccess> {
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
    if !crate::contribution::configured(repo) {
        ensure_origin(repo, &cwd)?;
    }

    let sha = rev_parse(&cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature branch HEAD", repo.id))?;
    let remote = crate::contribution::push_remote(&cwd, repo)?;
    let fork_source = if crate::contribution::cross_repository(repo)? {
        crate::contribution::source(repo)
    } else {
        None
    };
    run_push_to_source(
        &cwd,
        &remote,
        branch,
        set_upstream,
        force,
        fork_source,
        Some((&active.bundle, repo)),
    )
    .with_context(|| format!("{}: failed to push {branch}", repo.id))?;

    if set_upstream {
        crate::contribution::track_source(&cwd, repo, branch)?;
    }
    let upstream = if crate::contribution::configured(repo) {
        format!("{remote}/{branch}")
    } else if set_upstream {
        read_upstream(&cwd).unwrap_or_else(|| format!("origin/{branch}"))
    } else {
        format!("origin/{branch}")
    };
    Ok(PushSuccess { upstream, sha })
}

fn ensure_feature_branch(repo: &RepoEntry, expected: &str, cwd: &Path) -> Result<()> {
    let actual = current_branch(cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != expected {
        bail!(
            "{}: push expected feature branch `{expected}`, found `{actual}` in {}.",
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

/// The one `git push` door for Knit's fan-out commands (`knit push`,
/// `knit publish create`, the branch/artifact coupling below).
///
/// Every push is bounded by `KNIT_GIT_PUSH_TIMEOUT` (default 300s) so a
/// stalled connection cannot hold the command open, and a push that failed on
/// the way to the remote — a reset connection, a hung-up remote, a timeout —
/// is retried up to [`crate::retry::GIT_PUSH_ATTEMPTS`] times. A push the
/// remote *answered* (rejected, stale lease, refused credentials) is returned
/// immediately: that is an answer, and repeating it only delays it.
pub(crate) fn run_push_to(
    cwd: &Path,
    remote: &str,
    branch: &str,
    set_upstream: bool,
    force: PushForce,
) -> Result<()> {
    run_push_to_source(cwd, remote, branch, set_upstream, force, None, None)
}

fn run_push_to_source(
    cwd: &Path,
    remote: &str,
    branch: &str,
    set_upstream: bool,
    force: PushForce,
    recorded_source: Option<&str>,
    context: Option<(&ChangeGroup, &RepoEntry)>,
) -> Result<()> {
    let timeout = crate::retry::git_push_timeout()?;
    let tracking = crate::contribution::push_tracking(cwd, remote, branch, recorded_source)?;
    // Resolve and validate once: a retry must never acquire a newer lease.
    let lease = if force.wants_lease() {
        Some(resolve_push_lease(
            cwd,
            remote,
            branch,
            tracking.as_ref(),
            context,
        )?)
    } else {
        None
    };
    let pushed_sha = tracking
        .as_ref()
        .map(|_| rev_parse(cwd, &format!("refs/heads/{branch}")))
        .transpose()?;
    let result = crate::retry::retry_transient(
        "push",
        crate::retry::GIT_PUSH_ATTEMPTS,
        crate::retry::classify_git_push,
        || {
            let mut args = vec![OsString::from("push")];
            if set_upstream {
                args.push(OsString::from("--set-upstream"));
            }
            if let Some(force_arg) = lease
                .as_ref()
                .map(|lease| lease.argument.as_str())
                .or_else(|| force.git_arg())
            {
                args.push(OsString::from(force_arg));
            }
            args.push(OsString::from(remote));
            args.push(OsString::from(branch));

            git_output_with_timeout(cwd, args, timeout)?;
            Ok(())
        },
    );
    result.map_err(|error| {
        if let Some(lease) = &lease {
            let message = format!("{error:#}");
            let lower = message.to_ascii_lowercase();
            if lower.contains("stale info") || lower.contains("cannot lock ref")
                || lower.contains("lease rejected") || lower.contains("lease rejection")
            {
                return anyhow!(
                    "remote branch {branch} changed; fetch and inspect it with `git fetch {} {branch}` and integrate it before forcing: {}",
                    lease.push_url,
                    message.replace(&lease.raw_push_url, &lease.push_url)
                );
            }
        }
        error
    })?;
    if let (Some(tracking), Some(sha)) = (tracking, pushed_sha) {
        git_output(cwd, ["update-ref", &tracking.reference, &sha])?;
        git_output(cwd, ["update-ref", &tracking.role_reference, &sha])?;
    }
    Ok(())
}

struct PushLease {
    argument: String,
    raw_push_url: String,
    push_url: String,
}

fn resolve_push_lease(
    cwd: &Path,
    remote: &str,
    branch: &str,
    tracking: Option<&crate::contribution::PushTracking>,
    context: Option<(&ChangeGroup, &RepoEntry)>,
) -> Result<PushLease> {
    let raw_push_url = crate::contribution::git_remote_url(cwd, remote, true)?;
    let push_url = sanitized_push_url(&raw_push_url);
    let reference = format!("refs/heads/{branch}");
    let tip = remote_ref_sha(cwd, &raw_push_url, &reference)
        .map_err(|error| anyhow!("{}", format!("{error:#}").replace(&raw_push_url, &push_url)))?;
    if let Some(tip) = tip.as_deref() {
        let receipt = tracking.and_then(|tracking| tracking.expected.as_deref());
        if receipt != Some(tip) {
            let recorded =
                context.is_some_and(|(bundle, repo)| bundle_recorded_tip(bundle, repo, tip));
            let had_tip = if recorded {
                false
            } else {
                git_output(cwd, ["log", "-g", "--format=%H", &reference])?
                    .lines()
                    .any(|sha| sha == tip)
            };
            if !recorded && !had_tip {
                let repo_id = context.map(|(_, repo)| repo.id.as_str()).unwrap_or(remote);
                bail!("{repo_id}: {branch} on {push_url} is at {tip}, which this bundle never recorded and this checkout never had — someone else may have pushed. Inspect it with `git fetch {push_url} {branch}` and integrate it before forcing.");
            }
            crate::retry::note(format!(
                "no Knit push receipt for {branch}; leasing against the remote tip {} this bundle recorded",
                short_sha(tip)
            ));
        }
    }
    Ok(PushLease {
        argument: format!(
            "--force-with-lease={reference}:{}",
            tip.as_deref().unwrap_or_default()
        ),
        raw_push_url,
        push_url,
    })
}

fn bundle_recorded_tip(bundle: &ChangeGroup, repo: &RepoEntry, tip: &str) -> bool {
    repo.head_sha.as_deref() == Some(tip)
        || repo.base_sha.as_deref() == Some(tip)
        || bundle
            .commit_groups
            .iter()
            .flat_map(|group| &group.commits)
            .any(|commit| commit.repo_id == repo.id && commit.sha == tip)
        || bundle.nodes.iter().any(|node| {
            node.commits
                .iter()
                .any(|commit| commit.repo_id == repo.id && commit.sha == tip)
                || node.repo_changes.iter().any(|change| {
                    change.repo_id == repo.id
                        && (change.before_sha.as_deref() == Some(tip)
                            || change.after_sha == tip
                            || change
                                .commits
                                .iter()
                                .chain(&change.dropped_commits)
                                .any(|sha| sha == tip))
                })
        })
}

fn sanitized_push_url(remote: &str) -> String {
    if let Ok(mut url) = url::Url::parse(remote) {
        if url.has_host() {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            return url.to_string();
        }
    }
    remote.to_owned()
}

fn read_upstream(cwd: &Path) -> Option<String> {
    git_output(
        cwd,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
}

/// Ensure an open bundle's feature branches are on git `origin` before its
/// artifact is allowed onto a sync remote: "pushing a bundle" always means
/// branches and artifact together. Branches that are missing or stale on origin are
/// pushed (plain, never forced) from the bundle's checkout; when no checkout
/// exists the branch is only verified against the bundle's recorded head.
/// Terminal-state bundles (closed/archived/deleted) are a no-op — their
/// branches were published before landing/archiving.
///
/// Returns one human-readable line per branch that was actually pushed, in
/// the same shape as `knit push` output, so callers can print them.
pub(crate) fn ensure_open_bundle_branches_on_origin(
    root: &Path,
    bundle: &ChangeGroup,
) -> Result<Vec<String>> {
    if !matches!(bundle.state, None | Some(BundleState::Open)) {
        return Ok(Vec::new());
    }

    ensure_no_pending_rewrite(root, bundle)?;
    crate::contribution::validate_bundle(bundle)?;
    for repo in &bundle.repos {
        if let Some(cwd) = branch_push_dir(root, repo) {
            let mut resolved = repo.clone();
            if crate::contribution::discover(&cwd, &mut resolved)? {
                bail!("{}: contribution identity is not recorded; run knit push or publish create for this repository before syncing", repo.id);
            }
            if crate::contribution::configured(repo) {
                crate::contribution::push_remote(&cwd, repo)?;
            }
        }
    }
    let mut pushed = Vec::new();
    for repo in &bundle.repos {
        // No git remote recorded: the branch/artifact coupling cannot apply.
        let Some(remote_url) = crate::contribution::source(repo) else {
            continue;
        };
        let Some(branch) = repo.feature_branch.as_deref() else {
            continue;
        };
        let reference = format!("refs/heads/{branch}");

        let Some(cwd) = branch_push_dir(root, repo) else {
            // Artifact-only workspace (e.g. a pulled bundle without local
            // checkouts): nothing to push from, so verification-only against
            // the recorded remote URL.
            let remote_sha = remote_ref_sha(root, remote_url, &reference).with_context(|| {
                format!(
                    "repo {}: git remote is unreachable, so feature branch {branch} cannot be verified on origin",
                    repo.id
                )
            })?;
            verify_branch_at_recorded_head(bundle, repo, branch, remote_sha)?;
            continue;
        };

        let local_tip = ref_commit_sha(&cwd, branch).with_context(|| {
            format!(
                "repo {}: failed to resolve feature branch {branch}",
                repo.id
            )
        })?;
        let push_remote = crate::contribution::push_remote(&cwd, repo)?;
        let verify_remote = if crate::contribution::configured(repo) {
            remote_url
        } else {
            "origin"
        };
        let remote_sha = remote_ref_sha(&cwd, verify_remote, &reference).with_context(|| {
            format!(
                "repo {}: origin is unreachable, so feature branch {branch} cannot be verified",
                repo.id
            )
        })?;

        let Some(local_tip) = local_tip else {
            // The checkout exists but the branch does not (yet): fall back to
            // verifying origin against the recorded head.
            verify_branch_at_recorded_head(bundle, repo, branch, remote_sha)?;
            continue;
        };
        if remote_sha.as_deref() == Some(local_tip.as_str()) {
            continue;
        }
        run_push_to(&cwd, &push_remote, branch, true, PushForce::No).map_err(|error| {
            anyhow!(
                "repo {}: feature branch {branch} is not on origin and could not be pushed: {error:#}",
                repo.id
            )
        })?;
        crate::contribution::track_source(&cwd, repo, branch)?;
        pushed.push(format!(
            "{}: {} {} {}",
            out::repo(&repo.id),
            out::movement("pushed"),
            out::branch(format!("origin/{branch}")),
            out::sha(short_sha(&local_tip))
        ));
    }

    Ok(pushed)
}

/// Where a bundle repo's feature branch can be pushed from: the recorded
/// worktree when it exists, else the source repo checkout (git worktree
/// branches live in the shared ref store, so pushing from the source repo
/// moves the same branch).
fn branch_push_dir(root: &Path, repo: &RepoEntry) -> Option<PathBuf> {
    if let Some(path) = &repo.worktree_path {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        if path.exists() {
            return Some(path);
        }
    }
    if repo.path.is_empty() {
        return None;
    }
    let path = PathBuf::from(&repo.path);
    path.exists().then_some(path)
}

/// Verification-only branch gate for repos without a pushable checkout: the
/// branch must exist on origin at the bundle's recorded head.
fn verify_branch_at_recorded_head(
    bundle: &ChangeGroup,
    repo: &RepoEntry,
    branch: &str,
    remote_sha: Option<String>,
) -> Result<()> {
    let Some(remote_sha) = remote_sha else {
        bail!(
            "repo {}: feature branch {branch} is missing on origin and there is no local checkout to push it from",
            repo.id
        );
    };
    match latest_recorded_head_sha(bundle, repo) {
        Some(recorded) if recorded != remote_sha => bail!(
            "repo {}: feature branch {branch} on origin is at {} but the bundle records {}, and there is no local checkout to push from",
            repo.id,
            short_sha(&remote_sha),
            short_sha(&recorded)
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{bundle_recorded_tip, sanitized_push_url, PushForce};
    use crate::model::ChangeGroup;
    use serde_json::json;

    const TIP: &str = "1111111111111111111111111111111111111111";
    const LEDGER_LOCATIONS: &[&str] = &[
        "headSha",
        "baseSha",
        "commitGroups",
        "nodeCommits",
        "commits",
        "droppedCommits",
        "beforeSha",
        "afterSha",
    ];

    fn recorded_tip_fixture(location: &str, recording_repo: &str) -> ChangeGroup {
        let repo = |id| {
            json!({
                "id": id,
                "path": "",
                "remote": null,
                "baseBranch": "main",
                "featureBranch": "knit/example",
                "worktreePath": null
            })
        };
        let mut bundle = json!({
            "schemaVersion": "0.1",
            "kind": "ChangeGroup",
            "id": "example",
            "title": "Example",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "repos": [repo("api"), repo("web")],
            "commitGroups": [],
            "nodes": []
        });
        let commit = json!({"repoId": recording_repo, "sha": TIP});
        let mut node = json!({
            "id": "observation",
            "type": "repo.observed",
            "createdAt": "2026-01-01T00:00:00Z",
            "commits": [],
            "repoChanges": []
        });
        match location {
            "headSha" | "baseSha" => {
                let index = if recording_repo == "api" { 0 } else { 1 };
                bundle["repos"][index][location] = json!(TIP);
            }
            "commitGroups" => {
                bundle["commitGroups"] = json!([{
                    "id": "group",
                    "message": "Recorded commit",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "commits": [commit]
                }]);
            }
            "nodeCommits" => node["commits"] = json!([commit]),
            "commits" | "droppedCommits" | "beforeSha" | "afterSha" => {
                let mut change = json!({
                    "repoId": recording_repo,
                    "beforeSha": null,
                    "afterSha": "2222222222222222222222222222222222222222",
                    "commits": [],
                    "droppedCommits": []
                });
                change[location] = if matches!(location, "commits" | "droppedCommits") {
                    json!([TIP])
                } else {
                    json!(TIP)
                };
                node["repoChanges"] = json!([change]);
            }
            _ => panic!("unknown ledger location: {location}"),
        }
        bundle["nodes"] = json!([node]);
        serde_json::from_value(bundle).unwrap()
    }

    #[test]
    fn recorded_tip_accepts_each_ledger_location() {
        for &location in LEDGER_LOCATIONS {
            let bundle = recorded_tip_fixture(location, "api");
            assert!(
                bundle_recorded_tip(&bundle, &bundle.repos[0], TIP),
                "did not accept {location}"
            );
            assert!(
                !bundle_recorded_tip(&bundle, &bundle.repos[0], "unrecorded"),
                "accepted an unrecorded tip in {location}"
            );
        }
    }

    #[test]
    fn recorded_tip_rejects_each_ledger_location_for_another_repo() {
        for &location in LEDGER_LOCATIONS {
            let bundle = recorded_tip_fixture(location, "web");
            assert!(
                !bundle_recorded_tip(&bundle, &bundle.repos[0], TIP),
                "accepted another repo's {location}"
            );
        }
    }

    #[test]
    fn push_url_sanitization_removes_credentials_and_query() {
        for remote in [
            "https://username@example.test/team/repo.git",
            "https://:password@example.test/team/repo.git",
            "https://example.test/team/repo.git?token=secret",
            "https://username:password@example.test/team/repo.git?token=secret#secret",
        ] {
            assert_eq!(
                sanitized_push_url(remote),
                "https://example.test/team/repo.git"
            );
        }
        assert_eq!(
            sanitized_push_url("git@example.test:team/repo.git"),
            "git@example.test:team/repo.git"
        );
    }

    #[test]
    fn from_flags_maps_the_flag_pair() {
        assert_eq!(PushForce::from_flags(false, false), PushForce::No);
        assert_eq!(PushForce::from_flags(true, false), PushForce::WithLease);
        assert_eq!(PushForce::from_flags(false, true), PushForce::Unconditional);
    }

    #[test]
    fn force_and_lease_predicates() {
        assert!(!PushForce::No.is_force());
        assert!(PushForce::WithLease.is_force());
        assert!(PushForce::Unconditional.is_force());
        assert!(PushForce::WithLease.wants_lease());
        assert!(!PushForce::Unconditional.wants_lease());
        assert!(!PushForce::No.wants_lease());
    }
}
