pub mod forgejo;
pub mod github;
pub mod gitlab;

use crate::model::{ChangeGroup, PublicationEntry, RepoEntry};
use crate::output as out;
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Once;
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
            let runs = match self.check_runs(target, selector, required_only) {
                Ok(runs) => runs,
                Err(err) if is_gh_checks_access_error(&err) => Vec::new(),
                Err(err) => return Err(err),
            };
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
///
/// For `gh`, an invalid `GITHUB_TOKEN` or `GH_TOKEN` in the environment overrides
/// `gh auth login`. When a host-token call fails with an auth error, Knit retries
/// once without those variables so interactive credentials can succeed.
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

    match run_cli_output(bin, cwd, &args, stdin, false) {
        Ok(output) => Ok(output),
        Err(first) if should_retry_gh_without_env_token(bin, &first) => {
            match run_cli_output(bin, cwd, &args, stdin, true) {
                Ok(output) => {
                    warn_gh_env_token_override();
                    Ok(output)
                }
                Err(retry) => Err(enhance_gh_auth_error(retry)),
            }
        }
        Err(err) => Err(if bin == "gh" {
            enhance_gh_auth_error(err)
        } else {
            err
        }),
    }
}

/// Spawn a forge CLI by name. On Windows, `Command::new` resolves `.exe` only,
/// missing `.cmd`/`.bat` shims (common for npm- or scoop-installed CLIs) — and
/// probing extensions globally would let a real `gh.exe` late in PATH shadow a
/// `gh.cmd` early in PATH. Resolve PATH ourselves so directory order wins
/// first and extension order (`exe`, `cmd`, `bat`) second, matching how
/// cmd.exe itself resolves commands.
fn forge_cli_command(bin: &str) -> Command {
    #[cfg(windows)]
    {
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                for extension in ["exe", "cmd", "bat"] {
                    let candidate = dir.join(format!("{bin}.{extension}"));
                    if candidate.is_file() {
                        return Command::new(candidate);
                    }
                }
            }
        }
    }
    Command::new(bin)
}

fn run_cli_output(
    bin: &str,
    cwd: &Path,
    args: &[OsString],
    stdin: Option<&str>,
    strip_host_tokens: bool,
) -> Result<String> {
    let mut command = forge_cli_command(bin);
    command
        .args(args)
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
        if strip_host_tokens {
            command.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
        }
    }
    let mut child = command
        .spawn()
        .with_context(|| {
            format!(
                "failed to run `{bin} {}` in {}. Install and authenticate `{bin}` to use this Knit code host provider.",
                display_args(args),
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
        .with_context(|| format!("failed to wait for `{bin} {}`", display_args(args)))?;

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
        display_args(args),
        cwd.display(),
        detail
    );
}

fn gh_env_token_vars() -> Vec<&'static str> {
    let mut vars = Vec::new();
    if std::env::var_os("GH_TOKEN")
        .is_some_and(|value| !value.is_empty())
    {
        vars.push("GH_TOKEN");
    }
    if std::env::var_os("GITHUB_TOKEN")
        .is_some_and(|value| !value.is_empty())
    {
        vars.push("GITHUB_TOKEN");
    }
    vars
}

fn should_retry_gh_without_env_token(bin: &str, err: &anyhow::Error) -> bool {
    bin == "gh" && !gh_env_token_vars().is_empty() && looks_like_gh_auth_failure(&err.to_string())
}

pub(crate) fn is_gh_checks_access_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let message = cause.to_string().to_ascii_lowercase();
        message.contains("statuscheckrollup")
            || message.contains("resource not accessible")
            || message.contains("insufficient_scope")
            || (message.contains("graphql") && message.contains("not accessible"))
            || (message.contains("gh pr checks") && message.contains("failed"))
    })
}

fn looks_like_gh_auth_failure(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("401")
        || lower.contains("bad credentials")
        || lower.contains("authentication failed")
        || lower.contains("not authenticated")
        || (lower.contains("403") && lower.contains("denied"))
}

pub(crate) fn is_likely_host_auth_failure(err: &anyhow::Error) -> bool {
    looks_like_gh_auth_failure(&err.to_string())
}

fn enhance_gh_auth_error(err: anyhow::Error) -> anyhow::Error {
    let vars = gh_env_token_vars();
    if vars.is_empty() || !looks_like_gh_auth_failure(&err.to_string()) {
        return err;
    }
    let names = vars.join(" and ");
    anyhow::anyhow!(
        "{err:#}\nHint: `{names}` override `gh auth login`. Run `unset GH_TOKEN GITHUB_TOKEN`, then `gh auth login -h github.com`, or fix the token value."
    )
}

static GH_ENV_TOKEN_WARNING: Once = Once::new();

fn warn_gh_env_token_override() {
    GH_ENV_TOKEN_WARNING.call_once(|| {
        let vars = gh_env_token_vars().join(" and ");
        eprintln!(
            "{}",
            out::warn(format!(
                "Ignored invalid {vars} for `gh`; retried with `gh auth login` credentials. Unset {vars} in your shell profile to avoid this."
            ))
        );
    });
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

    #[test]
    fn detects_gh_auth_failures() {
        assert!(looks_like_gh_auth_failure(
            "HTTP 401: Bad credentials (https://api.github.com/graphql)"
        ));
        assert!(looks_like_gh_auth_failure("authentication failed"));
        assert!(!looks_like_gh_auth_failure("graphQL: Could not resolve to a PullRequest"));
    }
}
