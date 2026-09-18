pub mod bitbucket;
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

/// What a host-side branch merge did. Landing into the same environment twice
/// is a no-op, not an error, so "already contained" is an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchMergeStatus {
    Merged,
    AlreadyContained,
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
    /// Change the destination branch of an existing open review object.
    fn edit_base(&self, _target: &PrTarget, _selector: &str, _base: &str) -> Result<()> {
        bail!(
            "{} does not support changing an existing review object's target branch in Knit yet; publish it against the intended branch instead.",
            self.id()
        )
    }
    fn merge(
        &self,
        target: &PrTarget,
        selector: &str,
        method: &str,
        delete_branch: bool,
        match_head: Option<&str>,
    ) -> Result<()>;
    /// Merge one branch into another on the host, without a review object and
    /// without a local checkout. How a bundle reaches an environment when the
    /// caller has only the bundle artifact: the feature branch goes into the
    /// environment branch and the review stays open where it is.
    fn merge_branch(
        &self,
        _target: &PrTarget,
        _base: &str,
        _head: &str,
    ) -> Result<BranchMergeStatus> {
        bail!(
            "{} cannot merge one branch into another from a bundle artifact in Knit yet. Land this lane from a workspace with `knit land --lane`, which merges in a local checkout instead.",
            self.id()
        )
    }
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
                // The lenient access-error fallback matches gh-shaped error
                // text; other forges embed raw response bodies, so treating
                // their failures as "no checks" would skip the CI gate.
                Err(err) if self.id() == "github" && is_gh_checks_access_error(&err) => Vec::new(),
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
/// An explicit project credential selects the adapter using metadata only.
/// Without assignments, detect known hosts and default to GitHub for other
/// remotes, preserving Knit's original `gh`-backed behavior.
pub fn for_repo(repo: &RepoEntry) -> Result<Box<dyn Forge>> {
    if let Some(remote) = &repo.remote {
        if let Some(provider) = crate::auth::provider_for_remote(&std::env::current_dir()?, remote)?
        {
            return by_id(&provider)
                .with_context(|| format!("unsupported credential provider `{provider}`"));
        }
    }
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
        "bitbucket" => Some(Box::new(bitbucket::Bitbucket)),
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
    } else if host == "bitbucket.org" || host.contains("bitbucket") {
        Some(Box::new(bitbucket::Bitbucket))
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

/// Insert or update the recorded review publication for a repo. Returns
/// whether the bundle actually changed: a refresh that reports exactly the
/// recorded review leaves the artifact untouched (including its timestamps),
/// so callers can skip rewriting it.
pub fn upsert_publication(
    bundle: &mut ChangeGroup,
    repo: &RepoEntry,
    forge: &dyn Forge,
    pr: &PullRequest,
) -> bool {
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
        let unchanged = existing.provider == entry.provider
            && existing.kind == entry.kind
            && existing.number == entry.number
            && existing.url == entry.url
            && existing.base_branch == entry.base_branch
            && existing.head_branch == entry.head_branch
            && existing.state == entry.state
            && existing.title == entry.title;
        if unchanged {
            return false;
        }
        *existing = entry;
    } else {
        bundle.publications.push(entry);
    }
    bundle.updated_at = now_iso();
    true
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

/// Resolve the actual operation target, including artifact-mode repositories.
/// An explicit repository must never inherit the checkout origin's binding.
pub(crate) fn target_credential(
    target: &PrTarget,
    provider: &str,
) -> Result<Option<crate::auth::ResolvedCredential>> {
    let credential = match &target.repo_full_name {
        Some(repo) => crate::auth::resolve_repository(&target.cwd, provider, repo)?,
        None => crate::auth::resolve(&target.cwd, None)?,
    };
    if let Some(credential) = &credential {
        if credential.provider != provider {
            bail!(
                "credential `{}` is for {}, but this operation uses {provider}",
                credential.name,
                credential.provider
            );
        }
    }
    Ok(credential)
}

/// Bound secrets only go to the expected HTTPS API origin. Environment API
/// overrides remain supported for legacy authentication, but cannot redirect a
/// project's saved token to another server.
pub(crate) fn bound_api_base(credential: &crate::auth::ResolvedCredential) -> Result<String> {
    let host = &credential.host;
    let base = match credential.provider.as_str() {
        "github" if host == "github.com" => "https://api.github.com".to_string(),
        "github" => format!("https://{host}/api/v3"),
        "gitlab" => format!("https://{host}/api/v4"),
        "bitbucket" if host == "bitbucket.org" => "https://api.bitbucket.org/2.0".to_string(),
        "bitbucket" => {
            bail!("project credentials currently support Bitbucket Cloud (bitbucket.org)")
        }
        "forgejo" => format!("https://{host}/api/v1"),
        provider => bail!("unsupported credential provider `{provider}`"),
    };
    let parsed = url::Url::parse(&base).context("invalid credential API host")?;
    let expected_host = match (credential.provider.as_str(), host.as_str()) {
        ("github", "github.com") => "api.github.com",
        ("bitbucket", "bitbucket.org") => "api.bitbucket.org",
        _ => host,
    };
    if parsed.host_str() != Some(expected_host)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!("invalid credential API host");
    }
    Ok(base)
}

fn configure_cli_credential(
    command: &mut Command,
    bin: &str,
    credential: &crate::auth::ResolvedCredential,
) -> Result<()> {
    match bin {
        "gh" => {
            command
                .env("GH_HOST", &credential.host)
                .env_remove("GH_TOKEN")
                .env_remove("GITHUB_TOKEN")
                .env_remove("GH_ENTERPRISE_TOKEN")
                .env_remove("GITHUB_ENTERPRISE_TOKEN")
                .env_remove("GH_REPO");
            if credential.host == "github.com" || credential.host.ends_with(".ghe.com") {
                command
                    .env("GH_TOKEN", &credential.token)
                    .env("GITHUB_TOKEN", &credential.token);
            } else {
                command
                    .env("GH_ENTERPRISE_TOKEN", &credential.token)
                    .env("GITHUB_ENTERPRISE_TOKEN", &credential.token);
            }
        }
        "glab" => {
            command
                .env("GITLAB_HOST", &credential.host)
                .env("GL_HOST", &credential.host)
                .env("GITLAB_TOKEN", &credential.token)
                .env("GLAB_TOKEN", &credential.token)
                .env("OAUTH_TOKEN", &credential.token)
                .env("GITLAB_API_HOST", &credential.host)
                .env("GLAB_API_PROTOCOL", "https")
                .env("API_PROTOCOL", "https")
                .env_remove("GITLAB_REPO")
                .env_remove("GITLAB_GROUP")
                .env_remove("GLAB_REPO");
        }
        _ => bail!("project credentials require the native API for `{bin}`"),
    }
    Ok(())
}

/// Run a forge CLI and capture stdout, returning a helpful error when the tool
/// is missing or exits non-zero.
///
/// For `gh`, an invalid `GITHUB_TOKEN` or `GH_TOKEN` in the environment overrides
/// `gh auth login`. When a host-token call fails with an auth error, Knit retries
/// once without those variables so interactive credentials can succeed. Explicit
/// project credentials never take this fallback path.
///
/// Calls that fail because the host was momentarily unavailable are retried
/// with backoff; see [`crate::retry`] for what counts as transient.
pub(crate) fn cli_output<I, S>(
    bin: &str,
    target: &PrTarget,
    args: I,
    stdin: Option<&str>,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<_>>();

    let provider = match bin {
        "gh" => "github",
        "glab" => "gitlab",
        "tea" => "forgejo",
        _ => bail!("unsupported forge CLI `{bin}`"),
    };
    let credential = target_credential(target, provider)?;
    if let Some(credential) = &credential {
        // gh api otherwise derives its host from cwd in some invocation
        // contexts, even when this operation explicitly targets another repo.
        if bin == "gh" && args.first().is_some_and(|arg| arg == "api") {
            args.push(OsString::from("--hostname"));
            args.push(OsString::from(&credential.host));
        }
        if (bin == "gh" && args.first().is_some_and(|arg| arg == "pr"))
            || (bin == "glab" && args.first().is_some_and(|arg| arg == "mr"))
        {
            let repository = if let Some(repository) = &target.repo_full_name {
                repository.clone()
            } else {
                // Use origin, as credential resolution does. Forge CLIs may
                // otherwise choose an upstream remote in a fork checkout.
                let output = Command::new("git")
                    .args(["remote", "get-url", "origin"])
                    .current_dir(&target.cwd)
                    .output()
                    .context("Cannot inspect origin for the assigned forge target")?;
                if !output.status.success() {
                    bail!("Cannot resolve origin for the assigned forge target");
                }
                let remote = std::str::from_utf8(&output.stdout)
                    .context("Repository origin is not UTF-8")?;
                let (host, repository) = crate::auth::remote_target(remote.trim())?;
                if host != credential.host {
                    bail!("Repository origin changed after credential resolution");
                }
                repository
            };
            let repository = if bin == "gh" {
                format!("{}/{repository}", credential.host)
            } else {
                format!("https://{}/{repository}", credential.host)
            };
            if let Some(index) = args.iter().position(|arg| arg == "--repo") {
                let value = args
                    .get_mut(index + 1)
                    .context("Missing forge repository argument")?;
                *value = OsString::from(repository);
            } else {
                args.push(OsString::from("--repo"));
                args.push(OsString::from(repository));
            }
        }
    }

    // Forge CLIs are the single door to every code host Knit talks to, so
    // this is where a host that is briefly unavailable (5xx, a rate limit, a
    // dropped connection) is given another chance. Anything the host actually
    // decided — bad credentials, a missing repo, a rejected payload — is
    // returned on the first attempt.
    crate::retry::retry_transient(
        &cli_action_label(bin, &args),
        crate::retry::FORGE_ATTEMPTS,
        crate::retry::classify_forge,
        || cli_output_once(bin, &target.cwd, &args, stdin, credential.as_ref()),
    )
}

/// How a retried forge call names itself in the streamed retry line:
/// `gh pr create`, `glab mr list`, `gh api`.
fn cli_action_label(bin: &str, args: &[OsString]) -> String {
    let words = args
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .take_while(|arg| !arg.starts_with('-'))
        .take(2)
        .collect::<Vec<_>>();
    if words.is_empty() {
        bin.to_string()
    } else {
        format!("{bin} {}", words.join(" "))
    }
}

fn cli_output_once(
    bin: &str,
    cwd: &Path,
    args: &[OsString],
    stdin: Option<&str>,
    credential: Option<&crate::auth::ResolvedCredential>,
) -> Result<String> {
    match run_cli_output(bin, cwd, args, stdin, false, credential) {
        Ok(output) => Ok(output),
        Err(first) if credential.is_none() && should_retry_gh_without_env_token(bin, &first) => {
            match run_cli_output(bin, cwd, args, stdin, true, None) {
                Ok(output) => {
                    warn_gh_env_token_override();
                    Ok(output)
                }
                Err(retry) => Err(enhance_gh_auth_error(retry)),
            }
        }
        Err(err) => Err(if let Some(credential) = credential {
            anyhow::anyhow!("{err:#}\nProject credential: `{}`. Update its token or project assignment with `knit project auth`.", credential.name)
        } else if bin == "gh" {
            enhance_gh_auth_error(err)
        } else {
            err
        }),
    }
}

/// Spawn a forge CLI by name. On Windows, `Command::new` resolves `.exe` only,
/// missing `.cmd`/`.bat` shims (common for npm- or scoop-installed CLIs) — and
/// probing extensions globally would let a real `gh.exe` late in PATH shadow a
/// `gh.cmd` early in PATH. Resolve PATH ourselves so directory order wins.
/// Prefer a native executable, but also support extensionless shell scripts
/// through Git for Windows' `sh`; unlike batch shims, that path preserves
/// multiline PR bodies as a single argument.
fn forge_cli_command(bin: &str) -> Command {
    #[cfg(windows)]
    {
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let executable = dir.join(format!("{bin}.exe"));
                if executable.is_file() {
                    return Command::new(executable);
                }
                let shell_script = dir.join(bin);
                if shell_script.is_file() {
                    let mut command = Command::new("sh");
                    command.arg(shell_script);
                    return command;
                }
                for extension in ["cmd", "bat"] {
                    let shim = dir.join(format!("{bin}.{extension}"));
                    if shim.is_file() {
                        return Command::new(shim);
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
    credential: Option<&crate::auth::ResolvedCredential>,
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
    if let Some(credential) = credential {
        configure_cli_credential(&mut command, bin, credential)?;
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
        credential
            .map(|credential| credential.redact(detail))
            .unwrap_or_else(|| detail.to_string())
    );
}

fn gh_env_token_vars() -> Vec<&'static str> {
    let mut vars = Vec::new();
    if std::env::var_os("GH_TOKEN").is_some_and(|value| !value.is_empty()) {
        vars.push("GH_TOKEN");
    }
    if std::env::var_os("GITHUB_TOKEN").is_some_and(|value| !value.is_empty()) {
        vars.push("GITHUB_TOKEN");
    }
    vars
}

fn should_retry_gh_without_env_token(bin: &str, err: &anyhow::Error) -> bool {
    bin == "gh" && !gh_env_token_vars().is_empty() && looks_like_gh_auth_failure(&err.to_string())
}

/// Whether a failed review creation means "this review already exists".
///
/// A create that is retried after a lost reply hits this: the first attempt
/// did reach the host. Publishing then adopts the existing review instead of
/// reporting a failure for work that succeeded.
pub(crate) fn is_existing_review_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("already exists")
        || message.contains("a pull request for these commits already exists")
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
    mentions_http_status(&lower, "401")
        || lower.contains("bad credentials")
        || lower.contains("authentication failed")
        || lower.contains("not authenticated")
        || (mentions_http_status(&lower, "403") && lower.contains("denied"))
}

/// Whether `detail` mentions `status` as a standalone number — `HTTP 401:` —
/// rather than as part of something else. Forge CLI errors echo the failing
/// command, so a review URL ending in `/pull/401`, a commit sha, or a larger
/// number must not read as an authentication failure.
fn mentions_http_status(lower: &str, status: &str) -> bool {
    lower.match_indices(status).any(|(start, _)| {
        let before = lower[..start].chars().next_back();
        let after = lower[start + status.len()..].chars().next();
        let is_boundary = |c: Option<char>| {
            c.is_none_or(|c| {
                !(c.is_ascii_alphanumeric() || matches!(c, '/' | '#' | '.' | '-' | '_'))
            })
        };
        is_boundary(before) && is_boundary(after)
    })
}

/// Whether `err` looks like a host rejecting our credentials — or Knit having
/// none to offer — regardless of which forge produced it: gh-style failures,
/// raw HTTP 401/403 responses, each adapter's missing-token message, and
/// `tea` with no logged-in Forgejo host.
pub(crate) fn is_likely_host_auth_failure(err: &anyhow::Error) -> bool {
    let detail = format!("{err:#}").to_ascii_lowercase();
    looks_like_gh_auth_failure(&detail)
        || detail.contains("authentication requires")
        || detail.contains("api access requires")
        || detail.contains("no available login")
        || detail.contains("unauthorized")
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

    fn credential(provider: &str, host: &str, token: &str) -> crate::auth::ResolvedCredential {
        crate::auth::ResolvedCredential {
            name: "test".into(),
            provider: provider.into(),
            host: host.into(),
            username: String::new(),
            token_type: None,
            token: token.into(),
        }
    }

    #[test]
    fn bound_api_destinations_follow_credential_hosts() {
        for (provider, host, expected) in [
            ("github", "github.com", "https://api.github.com"),
            (
                "github",
                "github.example.test",
                "https://github.example.test/api/v3",
            ),
            (
                "gitlab",
                "gitlab.example.test",
                "https://gitlab.example.test/api/v4",
            ),
            (
                "bitbucket",
                "bitbucket.org",
                "https://api.bitbucket.org/2.0",
            ),
            (
                "forgejo",
                "forgejo.example.test",
                "https://forgejo.example.test/api/v1",
            ),
        ] {
            assert_eq!(
                bound_api_base(&credential(provider, host, "secret")).unwrap(),
                expected
            );
        }
        assert!(bound_api_base(&credential("github", "github.com@evil.test", "secret")).is_err());
    }

    #[test]
    fn cli_credentials_are_scoped_to_each_child_and_host_class() {
        let mut public = Command::new("gh");
        configure_cli_credential(
            &mut public,
            "gh",
            &credential("github", "github.com", "public-secret"),
        )
        .unwrap();
        let mut enterprise = Command::new("gh");
        configure_cli_credential(
            &mut enterprise,
            "gh",
            &credential("github", "github.example.test", "enterprise-secret"),
        )
        .unwrap();
        let public_env: std::collections::BTreeMap<_, _> = public.get_envs().collect();
        let enterprise_env: std::collections::BTreeMap<_, _> = enterprise.get_envs().collect();
        assert_eq!(
            public_env[OsStr::new("GH_TOKEN")],
            Some(OsStr::new("public-secret"))
        );
        assert_eq!(public_env[OsStr::new("GH_ENTERPRISE_TOKEN")], None);
        assert_eq!(enterprise_env[OsStr::new("GH_TOKEN")], None);
        assert_eq!(
            enterprise_env[OsStr::new("GH_ENTERPRISE_TOKEN")],
            Some(OsStr::new("enterprise-secret"))
        );
    }

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
        assert_eq!(
            for_remote("https://bitbucket.org/acme/x.git").map(|f| f.id()),
            Some("bitbucket")
        );
        assert!(for_remote("https://example.com/acme/x.git").is_none());
    }

    #[test]
    fn by_id_resolves_aliases() {
        assert_eq!(by_id("github").map(|f| f.id()), Some("github"));
        assert_eq!(by_id("codeberg").map(|f| f.id()), Some("forgejo"));
        assert_eq!(by_id("bitbucket").map(|f| f.id()), Some("bitbucket"));
    }

    #[test]
    fn detects_gh_auth_failures() {
        assert!(looks_like_gh_auth_failure(
            "HTTP 401: Bad credentials (https://api.github.com/graphql)"
        ));
        assert!(looks_like_gh_auth_failure("authentication failed"));
        assert!(!looks_like_gh_auth_failure(
            "graphQL: Could not resolve to a PullRequest"
        ));
        // gh echoes the failing command: a review numbered 401, a sha or a
        // path containing the digits is not a credential problem.
        assert!(!looks_like_gh_auth_failure(
            "gh pr view https://github.com/acme/backend/pull/401 failed in /work/backend: GraphQL: Could not resolve to a PullRequest"
        ));
        assert!(!looks_like_gh_auth_failure(
            "commit 7ab401c is not on the base branch"
        ));
        assert!(!looks_like_gh_auth_failure("HTTP 4010 is not a status"));
        assert!(looks_like_gh_auth_failure(
            "gh pr view https://github.com/acme/backend/pull/7 failed: HTTP 401: Requires authentication"
        ));
        assert!(looks_like_gh_auth_failure("HTTP 403: Resource denied"));
        assert!(!looks_like_gh_auth_failure(
            "gh pr view https://github.com/acme/backend/pull/403 failed: access denied to draft"
        ));
    }
}
