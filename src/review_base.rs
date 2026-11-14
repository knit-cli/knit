//! The repository and branch a repository's bundle work is integrated against.
//!
//! With a recorded review, that is the review's base: its branch, in the
//! contribution target repository when one is configured and in `origin`
//! otherwise. Without a review it is the configured base branch in the same
//! repository. `knit land update`, `knit rebase` and `knit workspace status`
//! all resolve it here.
use crate::contribution;
use crate::model::{ChangeGroup, RepoEntry};
use crate::providers::publication_for_repo;
use anyhow::{Context, Result};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewBase {
    /// The contribution target URL, or `origin`.
    pub remote: String,
    pub branch: String,
    /// The recorded review this base belongs to; `None` for the configured base.
    pub review_url: Option<String>,
}

/// The base of the repository's recorded review, if one is recorded.
pub fn recorded(bundle: &ChangeGroup, repo: &RepoEntry) -> Result<Option<ReviewBase>> {
    let Some(publication) = publication_for_repo(bundle, &repo.id) else {
        return Ok(None);
    };
    Ok(Some(ReviewBase {
        remote: remote(repo)?,
        branch: publication.base_branch.clone(),
        review_url: Some(publication.url.clone()),
    }))
}

/// The recorded review's base, falling back to the configured base branch.
pub fn effective(bundle: &ChangeGroup, repo: &RepoEntry) -> Result<ReviewBase> {
    if let Some(base) = recorded(bundle, repo)? {
        return Ok(base);
    }
    Ok(ReviewBase {
        remote: remote(repo)?,
        branch: repo.base_branch.clone(),
        review_url: None,
    })
}

fn remote(repo: &RepoEntry) -> Result<String> {
    if contribution::configured(repo) {
        Ok(contribution::destination(repo)
            .context("missing targetRemote")?
            .to_owned())
    } else {
        Ok("origin".into())
    }
}

impl ReviewBase {
    pub fn is_origin(&self) -> bool {
        self.remote == "origin"
    }

    /// The local ref this base is fetched into.
    pub fn tracking_ref(&self, repo: &RepoEntry) -> Result<String> {
        if contribution::configured(repo) {
            contribution::role_ref(repo, &self.branch, false)
        } else {
            Ok(format!("refs/remotes/origin/{}", self.branch))
        }
    }

    /// Fetch the base branch and return its tracking ref.
    pub fn fetch(&self, cwd: &Path, repo: &RepoEntry) -> Result<String> {
        if contribution::configured(repo) {
            return contribution::fetch_ref(cwd, repo, &self.branch, false);
        }
        let reference = self.tracking_ref(repo)?;
        crate::git::git_output(
            cwd,
            [
                "fetch",
                "--no-tags",
                "origin",
                &format!("+refs/heads/{}:{reference}", self.branch),
            ],
        )?;
        Ok(reference)
    }

    /// `origin/main`, or `github.com/owner/name main` for a contribution target.
    pub fn label(&self) -> String {
        if self.is_origin() {
            return format!("origin/{}", self.branch);
        }
        let repository = crate::auth::remote_target(&self.remote)
            .map(|(host, path)| format!("{host}/{path}"))
            .unwrap_or_else(|_| self.remote.clone());
        format!("{repository} {}", self.branch)
    }

    /// The repository holding the recorded review when it lies outside
    /// `origin` on the same host while no contribution target is recorded,
    /// so `origin` cannot be the review's base. `None` when it is inside or
    /// cannot be told apart.
    pub fn review_outside_origin(&self, cwd: &Path) -> Option<String> {
        if !self.is_origin() {
            return None;
        }
        let review = url::Url::parse(self.review_url.as_deref()?).ok()?;
        let origin = contribution::git_remote_url(cwd, "origin", false).ok()?;
        let (host, path) = crate::auth::remote_target(&origin).ok()?;
        if !review.host_str()?.eq_ignore_ascii_case(&host) {
            return None;
        }
        let review_path = review.path().trim_start_matches('/').to_ascii_lowercase();
        if review_path.starts_with(&format!("{}/", path.to_ascii_lowercase())) {
            return None;
        }
        let repository: Vec<_> = review
            .path_segments()?
            .take_while(|segment| {
                !matches!(
                    *segment,
                    "pull" | "pulls" | "pull-requests" | "-" | "merge_requests"
                )
            })
            .collect();
        Some(format!("{host}/{}", repository.join("/")))
    }

    /// The refusal for a review [`Self::review_outside_origin`] reports.
    pub fn outside_origin_error(&self, bundle: &str, repo: &RepoEntry, repository: &str) -> String {
        format!(
            "{}: review {} is in {repository}, but the bundle records no contribution target and `origin` is another repository, so `origin/{}` is not the review's base. Add a remote for it and record it, then retry:\n  git -C {} remote add upstream https://{repository}.git\n  knit --bundle {bundle} publish create {} --source-remote origin --target-remote upstream",
            repo.id,
            self.review_url.as_deref().unwrap_or_default(),
            self.branch,
            repo.path,
            repo.id
        )
    }

    /// Whether this base is `origin/<branch>` as seen from `cwd`.
    pub fn is_origin_branch(&self, cwd: &Path, branch: &str) -> bool {
        if self.branch != branch {
            return false;
        }
        if self.is_origin() {
            return true;
        }
        contribution::git_remote_url(cwd, "origin", false)
            .ok()
            .is_some_and(|url| contribution::same_repository(&url, &self.remote).unwrap_or(false))
    }
}
