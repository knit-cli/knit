pub mod forgejo;
pub mod github;
pub mod gitlab;

use crate::model::{ChangeGroup, PublicationEntry, RepoEntry};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Review object kinds recorded in `publications`.
pub const PULL_REQUEST_KIND: &str = "pull_request";
pub const MERGE_REQUEST_KIND: &str = "merge_request";

/// Canonical, provider-neutral view of a host review object (PR / MR).
///
/// The GitHub adapter deserializes `gh` JSON straight into this shape; other
/// adapters parse their own CLI JSON and build it explicitly.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub base_ref_name: Option<String>,
    #[serde(default)]
    pub head_ref_name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub is_draft: Option<bool>,
    #[serde(default)]
    pub head_ref_oid: Option<String>,
    /// Mergeability as reported by the host: `MERGEABLE`, `CONFLICTING`, `UNKNOWN`.
    #[serde(default)]
    pub mergeable: Option<String>,
    /// Finer merge-state hint (`CLEAN`, `DIRTY`, `BLOCKED`, `BEHIND`, ...). GitHub only.
    #[serde(default)]
    pub merge_state_status: Option<String>,
    /// Review decision: `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, or empty.
    #[serde(default)]
    pub review_decision: Option<String>,
}

impl PullRequest {
    /// True when the host reports the PR conflicts with its base branch.
    pub fn is_conflicting(&self) -> bool {
        self.mergeable.as_deref() == Some("CONFLICTING")
            || self.merge_state_status.as_deref() == Some("DIRTY")
    }
}

/// A single status/check result for a review object.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRun {
    pub name: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
}

pub struct CheckWaitSummary {
    pub status: String,
    pub runs: Vec<CheckRun>,
}

/// Where a forge operation should run.
///
/// `cwd` is a git checkout used to resolve the repository. `repo_full_name` is
/// set in artifact mode (no local feature checkout), so the adapter can target
/// the repo explicitly (e.g. `gh --repo owner/name`).
pub struct PrTarget {
    pub cwd: PathBuf,
    pub repo_full_name: Option<String>,
}

impl PrTarget {
    pub fn checkout(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            repo_full_name: None,
        }
    }

    pub fn explicit(cwd: impl Into<PathBuf>, repo_full_name: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            repo_full_name: Some(repo_full_name.into()),
        }
    }
}

/// A code host that exposes review objects through a CLI tool.
pub trait Forge {
    /// Stable provider id recorded on publications, e.g. `github`.
    fn id(&self) -> &'static str;
    /// Review object kind recorded on publications.
    fn review_kind(&self) -> &'static str;
    /// CLI binary this adapter shells out to, e.g. `gh`.
    fn cli(&self) -> &'static str;
    /// Parse the host project path (`owner/name`) from a git remote URL.
    fn repo_full_name(&self, remote: &str) -> Option<String>;

    fn find_existing(
        &self,
        target: &PrTarget,
        head: &str,
        base: &str,
    ) -> Result<Option<PullRequest>>;
    fn create(
        &self,
        target: &PrTarget,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<String>;
    fn view(&self, target: &PrTarget, selector: &str) -> Result<PullRequest>;
    fn edit_body(&self, target: &PrTarget, selector: &str, body: &str) -> Result<()>;
    fn merge(
        &self,
        target: &PrTarget,
        selector: &str,
        method: &str,
        delete_branch: bool,
        match_head: Option<&str>,
    ) -> Result<()>;
    fn revert_pull_request(
        &self,
        _target: &PrTarget,
        _selector: &str,
        _title: &str,
        _body: &str,
    ) -> Result<String> {
        bail!(
            "{} does not support provider-native PR revert in Knit yet.",
            self.id()
        );
    }
    fn check_runs(
        &self,
        target: &PrTarget,
        selector: &str,
        required_only: bool,
    ) -> Result<Vec<CheckRun>>;

    /// Poll `check_runs` until checks pass, fail, or time out. Shared by all adapters.
    fn wait_for_checks(
        &self,
        target: &PrTarget,
        selector: &str,
        required_only: bool,
        timeout_seconds: u64,
        interval_seconds: u64,
    ) -> Result<CheckWaitSummary> {
        let started = Instant::now();
        let timeout = Duration::from_secs(timeout_seconds);
        let interval = Duration::from_secs(interval_seconds.max(1));

        loop {
            let runs = self.check_runs(target, selector, required_only)?;
            match checks_state(&runs) {
                ChecksState::NoChecks => {
                    return Ok(CheckWaitSummary {
                        status: "passed (no required checks)".to_string(),
                        runs,
                    })
                }
                ChecksState::Passed => {
                    return Ok(CheckWaitSummary {
                        status: "passed".to_string(),
                        runs,
                    })
                }
                ChecksState::Failed(name) => bail!("check `{name}` failed for {selector}"),
                ChecksState::Pending => {
                    if started.elapsed() >= timeout {
                        bail!("timed out waiting for checks on {selector}");
                    }
                    thread::sleep(interval);
                }
            }
        }
    }
}

/// Resolve a forge adapter from a git remote URL.
pub fn for_remote(remote: &str) -> Option<Box<dyn Forge>> {
    by_host(&remote_host(remote)?)
}

/// Resolve the forge adapter for a tracked repo, using its recorded remote.
///
/// GitLab and Codeberg/Forgejo are detected from the remote host; every other
/// remote (including unrecognized hosts and local paths) defaults to GitHub,
/// preserving Knit's original `gh`-backed behavior.
pub fn for_repo(repo: &RepoEntry) -> Result<Box<dyn Forge>> {
    Ok(repo
        .remote
        .as_deref()
        .and_then(for_remote)
        .unwrap_or_else(|| Box::new(github::GitHub)))
}

/// Resolve a forge adapter from a stored provider id.
pub fn by_id(id: &str) -> Option<Box<dyn Forge>> {
    match id {
        "github" => Some(Box::new(github::GitHub)),
        "gitlab" => Some(Box::new(gitlab::GitLab)),
        "forgejo" | "codeberg" | "gitea" => Some(Box::new(forgejo::Forgejo)),
        _ => None,
    }
}

fn by_host(host: &str) -> Option<Box<dyn Forge>> {
    let host = host.to_ascii_lowercase();
    if host == "github.com" || host.starts_with("github.") {
        Some(Box::new(github::GitHub))
    } else if host == "gitlab.com" || host.contains("gitlab") {
        Some(Box::new(gitlab::GitLab))
    } else if host == "codeberg.org" || host.contains("forgejo") || host.contains("gitea") {
        Some(Box::new(forgejo::Forgejo))
    } else {
        None
    }
}

/// Extract the host from common git remote URL forms (https, ssh, scp-like).
pub(crate) fn remote_host(remote: &str) -> Option<String> {
    let remote = remote.trim();
    if remote.is_empty() {
        return None;
    }
    // scp-like form: git@host:owner/repo.git
    if let Some(rest) = remote.strip_prefix("git@") {
        return rest
            .split(':')
            .next()
            .map(str::to_string)
            .filter(|host| !host.is_empty());
    }
    // scheme://[user@]host[:port]/path
    let after_scheme = remote.split("://").nth(1).unwrap_or(remote);
    let after_at = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    let host = after_at.split(['/', ':']).next()?;
    (!host.is_empty()).then(|| host.to_string())
}

pub fn is_review_kind(kind: &str) -> bool {
    kind == PULL_REQUEST_KIND || kind == MERGE_REQUEST_KIND
}

/// Find the recorded review publication for a repo, regardless of provider.
///
/// Knit records at most one review object per repo per bundle, so a repo id is a
/// sufficient key.
pub fn publication_for_repo<'a>(
    bundle: &'a ChangeGroup,
    repo_id: &str,
) -> Option<&'a PublicationEntry> {
    bundle
        .publications
        .iter()
        .find(|publication| publication.repo_id == repo_id && is_review_kind(&publication.kind))
}

/// Insert or update the recorded review publication for a repo.
pub fn upsert_publication(
    bundle: &mut ChangeGroup,
    repo: &RepoEntry,
    forge: &dyn Forge,
    pr: &PullRequest,
) {
    let entry = PublicationEntry {
        repo_id: repo.id.clone(),
        provider: forge.id().to_string(),
        kind: forge.review_kind().to_string(),
        number: pr.number,
        url: pr.url.clone(),
        base_branch: pr
            .base_ref_name
            .clone()
            .unwrap_or_else(|| repo.base_branch.clone()),
        head_branch: pr
            .head_ref_name
            .clone()
            .or_else(|| repo.feature_branch.clone())
            .unwrap_or_default(),
        state: pr.state.clone().unwrap_or_else(|| "UNKNOWN".to_string()),
        title: pr.title.clone(),
        updated_at: now_iso(),
    };

    if let Some(existing) = bundle
        .publications
        .iter_mut()
        .find(|publication| publication.repo_id == repo.id && is_review_kind(&publication.kind))
    {
        *existing = entry;
    } else {
        bundle.publications.push(entry);
    }
    bundle.updated_at = now_iso();
}

pub fn pr_number_from_url(url: &str) -> Option<u64> {
    url.rsplit('/').next()?.parse().ok()
}

enum ChecksState {
    NoChecks,
    Passed,
    Pending,
    Failed(String),
}

fn checks_state(runs: &[CheckRun]) -> ChecksState {
    if runs.is_empty() {
        return ChecksState::NoChecks;
    }
    let mut has_pending = false;
    for run in runs {
        let bucket = run.bucket.as_deref().unwrap_or("");
        let state = run.state.as_deref().unwrap_or("");
        if matches!(bucket, "fail" | "cancel") || matches!(state, "FAILURE" | "CANCELLED") {
            return ChecksState::Failed(run.name.clone());
        }
        if !matches!(bucket, "pass" | "skipping") && !matches!(state, "SUCCESS" | "SKIPPED") {
            has_pending = true;
        }
    }
    if has_pending {
        ChecksState::Pending
    } else {
        ChecksState::Passed
    }
}

/// Run a forge CLI and capture stdout, returning a helpful error when the tool
/// is missing or exits non-zero.
pub(crate) fn cli_output<I, S>(
    bin: &str,
    cwd: &Path,
    args: I,
    stdin: Option<&str>,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<_>>();
    let mut command = Command::new(bin);
    command
        .args(&args)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if bin == "gh" {
        command
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1");
    }
    let mut child = command
        .spawn()
        .with_context(|| {
            format!(
                "failed to run `{bin} {}` in {}. Install and authenticate `{bin}` to use this Knit code host provider.",
                display_args(&args),
                cwd.display()
            )
        })?;

    if let Some(input) = stdin {
        let mut child_stdin = child
            .stdin
            .take()
            .with_context(|| format!("failed to open stdin for `{bin}`"))?;
        child_stdin
            .write_all(input.as_bytes())
            .with_context(|| format!("failed to write input to `{bin}`"))?;
        drop(child_stdin);
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for `{bin} {}`", display_args(&args)))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    bail!(
        "{bin} {} failed in {}: {}",
        display_args(&args),
        cwd.display(),
        detail
    );
}

/// Build CLI args, optionally suffixed with a `<repo_flag> <full_name>` pair.
pub(crate) fn repo_scoped_args(
    target: &PrTarget,
    repo_flag: &str,
    args: Vec<OsString>,
) -> Vec<OsString> {
    let mut full = Vec::with_capacity(args.len() + 2);
    full.extend(args);
    if let Some(full_name) = &target.repo_full_name {
        full.push(OsString::from(repo_flag));
        full.push(OsString::from(full_name));
    }
    full
}

pub(crate) fn parse_pr_url(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .rev()
        .find(|token| token.starts_with("https://") || token.starts_with("http://"))
        .map(|token| token.trim_matches(|ch| ch == '"' || ch == '\'').to_string())
}

fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_host_from_remote_forms() {
        assert_eq!(
            remote_host("https://github.com/acme/backend.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            remote_host("git@github.com:acme/backend.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            remote_host("ssh://git@gitlab.example.com:2222/acme/backend.git").as_deref(),
            Some("gitlab.example.com")
        );
        assert_eq!(remote_host("").as_deref(), None);
    }

    #[test]
    fn maps_known_hosts_to_providers() {
        assert_eq!(
            for_remote("https://github.com/acme/x.git").map(|f| f.id()),
            Some("github")
        );
        assert_eq!(
            for_remote("git@gitlab.com:acme/x.git").map(|f| f.id()),
            Some("gitlab")
        );
        assert_eq!(
            for_remote("https://codeberg.org/acme/x.git").map(|f| f.id()),
            Some("forgejo")
        );
        assert!(for_remote("https://example.com/acme/x.git").is_none());
    }

    #[test]
    fn by_id_resolves_aliases() {
        assert_eq!(by_id("github").map(|f| f.id()), Some("github"));
        assert_eq!(by_id("codeberg").map(|f| f.id()), Some("forgejo"));
        assert!(by_id("bitbucket").is_none());
    }
}
