use crate::checkout::checkout_dir;
use crate::git::{
    current_branch, git_output, git_output_optional, ref_commit_sha, remote_ref_sha, rev_parse,
};
use crate::ids::short_sha;
use crate::model::{BundleState, ChangeGroup, RepoEntry};
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{load_active_bundle_for_update, ActiveBundle};
use crate::tracking::latest_recorded_head_sha;
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

struct PushSuccess {
    upstream: String,
    sha: String,
}

/// How `git push` may move the remote branch. Mirrors git's own flags:
/// `WithLease` refuses when the remote moved since the last fetch.
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
            Self::WithLease => Some("--force-with-lease"),
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

pub fn push_repos(
    selectors: &[String],
    all: bool,
    set_upstream: bool,
    force: PushForce,
    remote: &[String],
    no_remote: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let results: Vec<(String, Result<PushSuccess>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = indexes
            .iter()
            .map(|&index| {
                let active = &active;
                let repo = &active.bundle.repos[index];
                let repo_id = repo.id.clone();
                scope.spawn(move || (repo_id, push_repo(active, repo, set_upstream, force)))
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("push worker thread panicked"))
            .collect()
    });

    let mut failures = Vec::new();
    for (repo_id, result) in results {
        match result {
            Ok(success) => {
                println!(
                    "{}: {} {} {}",
                    out::repo(&repo_id),
                    out::movement("pushed"),
                    out::branch(success.upstream),
                    out::sha(short_sha(&success.sha))
                );
            }
            Err(error) => {
                println!("{}: {}", out::repo(&repo_id), out::danger("push failed"));
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    if !failures.is_empty() {
        bail!("push failed:\n{}", failures.join("\n"));
    }

    // After git branches are pushed, also sync the bundle artifact to the
    // configured sync remote (default on; see `knit config set push-sync`).
    // The force mode carries over: a forced branch push implies the ledger
    // rewrite must be forced onto the sync remote too.
    crate::commands::remote::maybe_sync_bundle_to_remote(&mut active, remote, no_remote, force)?;

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
    ensure_origin(repo, &cwd)?;

    let sha = rev_parse(&cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature branch HEAD", repo.id))?;
    run_push(&cwd, branch, set_upstream, force)
        .with_context(|| format!("{}: failed to push {branch}", repo.id))?;

    let upstream = if set_upstream {
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

fn run_push(cwd: &Path, branch: &str, set_upstream: bool, force: PushForce) -> Result<()> {
    let mut args = vec![OsString::from("push")];
    if set_upstream {
        args.push(OsString::from("--set-upstream"));
    }
    if let Some(force_arg) = force.git_arg() {
        args.push(OsString::from(force_arg));
    }
    args.push(OsString::from("origin"));
    args.push(OsString::from(branch));

    git_output(cwd, args)?;
    Ok(())
}

fn read_upstream(cwd: &Path) -> Option<String> {
    git_output(
        cwd,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
}

/// Ensure an open bundle's feature branches are on git `origin` before its
/// artifact is allowed onto a sync remote: "pushing a bundle" means branches
/// + artifact, always. Branches that are missing or stale on origin are
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

    let mut pushed = Vec::new();
    for repo in &bundle.repos {
        // No git remote recorded: the branch/artifact coupling cannot apply.
        let Some(remote_url) = repo.remote.as_deref() else {
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
        let remote_sha = remote_ref_sha(&cwd, "origin", &reference).with_context(|| {
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
        run_push(&cwd, branch, true, PushForce::No).map_err(|error| {
            anyhow!(
                "repo {}: feature branch {branch} is not on origin and could not be pushed: {error:#}",
                repo.id
            )
        })?;
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
    use super::PushForce;

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
