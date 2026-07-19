use crate::checkout::checkout_dir;
use crate::git::{current_branch, git_output, git_output_optional, rev_parse};
use crate::ids::short_sha;
use crate::model::RepoEntry;
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{load_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::Path;

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
    let active = load_active_bundle()?;
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
    crate::commands::remote::maybe_sync_bundle_to_remote(remote, no_remote, force)?;

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
