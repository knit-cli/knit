//! Configured-base discovery, snapshotting, and safe local fast-forwards.

use crate::git::{
    current_branch, git_output, git_output_optional, git_root, is_ancestor, ref_commit_sha,
};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleBaseMode {
    /// Fetch `origin/<base>` and snapshot that remote-tracking commit.
    FreshRemote,
    /// Do not access the network; prefer cached `origin/<base>`, then local base.
    CachedRemote,
    /// Use the local configured base branch exactly as it stands.
    Local,
}

#[derive(Debug, Clone)]
pub struct BaseSnapshot {
    pub sha: String,
    pub source_ref: String,
}

#[derive(Debug, Clone)]
pub struct BaseInspection {
    pub current_branch: String,
    pub dirty: bool,
    pub local_sha: Option<String>,
    pub remote_sha: Option<String>,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BaseAdvance {
    pub before: Option<String>,
    pub after: String,
}

/// Validate an explicitly configured base without making network availability
/// a requirement. Prefer a freshly fetched remote branch, then cached remote
/// metadata, then a local branch.
pub fn validate_configured_base(repo_path: &Path, base_branch: &str) -> Result<BaseSnapshot> {
    let base_branch = base_branch.trim();
    if base_branch.is_empty() {
        bail!("Base branch cannot be empty.");
    }

    let repo_root = git_root(repo_path)?;
    if git_output_optional(&repo_root, ["check-ref-format", "--branch", base_branch])?.is_none() {
        bail!("`{base_branch}` is not a valid branch name.");
    }
    let remote_ref = format!("origin/{base_branch}");
    let has_origin = git_output_optional(&repo_root, ["remote", "get-url", "origin"])?.is_some();
    let mut fetch_error = None;

    if has_origin {
        match fetch_base(&repo_root, base_branch) {
            Ok(()) => {
                if let Some(sha) = ref_commit_sha(&repo_root, &remote_ref)? {
                    return Ok(BaseSnapshot {
                        sha,
                        source_ref: remote_ref,
                    });
                }
            }
            Err(error) => fetch_error = Some(error),
        }

        if let Some(sha) = ref_commit_sha(&repo_root, &remote_ref)? {
            return Ok(BaseSnapshot {
                sha,
                source_ref: format!("cached {remote_ref}"),
            });
        }
    }

    if let Some(sha) = ref_commit_sha(&repo_root, &format!("refs/heads/{base_branch}"))? {
        return Ok(BaseSnapshot {
            sha,
            source_ref: format!("local {base_branch}"),
        });
    }

    if let Some(error) = fetch_error {
        bail!(
            "Base branch `{base_branch}` was not found locally or in cached origin refs, and refreshing origin failed: {error:#}"
        );
    }
    bail!(
        "Base branch `{base_branch}` was not found locally{}.",
        if has_origin { " or on origin" } else { "" }
    );
}

pub fn snapshot_base(
    repo_path: &Path,
    base_branch: &str,
    mode: BundleBaseMode,
) -> Result<BaseSnapshot> {
    let repo_root = git_root(repo_path)?;
    let local_ref = base_branch.to_string();
    let remote_ref = format!("origin/{base_branch}");
    let has_origin = git_output_optional(&repo_root, ["remote", "get-url", "origin"])?.is_some();

    match mode {
        BundleBaseMode::FreshRemote if has_origin => {
            fetch_base(&repo_root, base_branch)?;
            let sha = ref_commit_sha(&repo_root, &remote_ref)?.with_context(|| {
                format!(
                    "configured base `{base_branch}` was not found on origin for {}",
                    repo_root.display()
                )
            })?;
            Ok(BaseSnapshot {
                sha,
                source_ref: remote_ref,
            })
        }
        BundleBaseMode::FreshRemote => local_snapshot(&repo_root, &local_ref).with_context(|| {
            format!(
                "{} has no origin remote; using its local configured base requires that `{base_branch}` exist",
                repo_root.display()
            )
        }),
        BundleBaseMode::CachedRemote => {
            if let Some(sha) = ref_commit_sha(&repo_root, &remote_ref)? {
                Ok(BaseSnapshot {
                    sha,
                    source_ref: remote_ref,
                })
            } else {
                local_snapshot(&repo_root, &local_ref)
            }
        }
        BundleBaseMode::Local => local_snapshot(&repo_root, &local_ref),
    }
}

fn local_snapshot(repo_root: &Path, local_ref: &str) -> Result<BaseSnapshot> {
    let sha = ref_commit_sha(repo_root, local_ref)?
        .with_context(|| format!("local configured base `{local_ref}` does not exist"))?;
    Ok(BaseSnapshot {
        sha,
        source_ref: local_ref.to_string(),
    })
}

pub fn fetch_base(repo_root: &Path, base_branch: &str) -> Result<()> {
    git_output_optional(repo_root, ["remote", "get-url", "origin"])?
        .with_context(|| format!("no `origin` remote configured in {}", repo_root.display()))?;
    // Remote-tracking refs mirror the remote, including intentional rewrites.
    // The leading `+` only permits replacing origin/<base>; advancing a local
    // base still has its own ancestry checks in `advance_local_base`.
    let source = format!("+refs/heads/{base_branch}");
    let destination = format!("refs/remotes/origin/{base_branch}");
    git_output(
        repo_root,
        [
            OsString::from("fetch"),
            OsString::from("origin"),
            OsString::from(format!("{source}:{destination}")),
        ],
    )
    .with_context(|| format!("failed to fetch origin/{base_branch}"))?;
    Ok(())
}

pub fn inspect_base(repo_path: &Path, base_branch: &str) -> Result<BaseInspection> {
    let repo_root = git_root(repo_path)?;
    let current_branch = current_branch(&repo_root)?.unwrap_or_else(|| "(detached)".to_string());
    let dirty = !git_output(&repo_root, ["status", "--short"])?
        .trim()
        .is_empty();
    let remote_ref = format!("origin/{base_branch}");
    let local_sha = ref_commit_sha(&repo_root, base_branch)?;
    let remote_sha = ref_commit_sha(&repo_root, &remote_ref)?;
    let (ahead, behind) = match (&local_sha, &remote_sha) {
        (Some(_), Some(_)) => {
            let range = format!("{base_branch}...{remote_ref}");
            let counts = git_output(&repo_root, ["rev-list", "--left-right", "--count", &range])?;
            let mut fields = counts.split_whitespace();
            let ahead = fields.next().and_then(|value| value.parse::<u64>().ok());
            let behind = fields.next().and_then(|value| value.parse::<u64>().ok());
            (ahead, behind)
        }
        _ => (None, None),
    };
    Ok(BaseInspection {
        current_branch,
        dirty,
        local_sha,
        remote_sha,
        ahead,
        behind,
    })
}

/// Fetch and fast-forward the configured local base without switching the
/// source checkout. If the base is checked out, its worktree must be clean.
pub fn advance_local_base(repo_path: &Path, base_branch: &str) -> Result<BaseAdvance> {
    let repo_root = git_root(repo_path)?;
    fetch_base(&repo_root, base_branch)?;
    let remote_ref = format!("origin/{base_branch}");
    let remote_sha = ref_commit_sha(&repo_root, &remote_ref)?
        .with_context(|| format!("origin/{base_branch} does not exist"))?;
    let local_sha = ref_commit_sha(&repo_root, base_branch)?;

    let Some(local_sha) = local_sha else {
        git_output(&repo_root, ["branch", base_branch, remote_ref.as_str()])?;
        return Ok(BaseAdvance {
            before: None,
            after: remote_sha,
        });
    };
    if local_sha == remote_sha {
        return Ok(BaseAdvance {
            before: Some(local_sha),
            after: remote_sha,
        });
    }
    if !is_ancestor(&repo_root, &local_sha, &remote_sha) {
        if is_ancestor(&repo_root, &remote_sha, &local_sha) {
            bail!(
                "local `{base_branch}` is ahead of origin/{base_branch}; refusing to discard local commits"
            );
        }
        bail!("local `{base_branch}` has diverged from origin/{base_branch}");
    }

    if let Some(checkout) = branch_checkout(&repo_root, base_branch)? {
        let status = git_output(&checkout, ["status", "--short"])?;
        if !status.trim().is_empty() {
            bail!(
                "configured base `{base_branch}` is checked out with uncommitted changes in {}",
                checkout.display()
            );
        }
        git_output(&checkout, ["merge", "--ff-only", remote_ref.as_str()])?;
    } else {
        let local_ref = format!("refs/heads/{base_branch}");
        git_output(
            &repo_root,
            [
                "update-ref",
                local_ref.as_str(),
                remote_sha.as_str(),
                local_sha.as_str(),
            ],
        )?;
    }

    Ok(BaseAdvance {
        before: Some(local_sha),
        after: remote_sha,
    })
}

fn branch_checkout(repo_root: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let output = git_output(repo_root, ["worktree", "list", "--porcelain"])?;
    let wanted = format!("refs/heads/{branch}");
    let mut worktree = None;
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            worktree = Some(PathBuf::from(path));
        } else if let Some(reference) = line.strip_prefix("branch ") {
            if reference == wanted {
                return Ok(worktree);
            }
        } else if line.is_empty() {
            worktree = None;
        }
    }
    Ok(None)
}
