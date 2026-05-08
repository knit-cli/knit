use crate::model::{ChangeGroup, PublicationEntry, RepoEntry};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const PROVIDER: &str = "github";
pub const PULL_REQUEST_KIND: &str = "pull_request";

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
}

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

pub fn publication_for_repo<'a>(
    bundle: &'a ChangeGroup,
    repo_id: &str,
) -> Option<&'a PublicationEntry> {
    bundle.publications.iter().find(|publication| {
        publication.provider == PROVIDER
            && publication.kind == PULL_REQUEST_KIND
            && publication.repo_id == repo_id
    })
}

pub fn upsert_publication(bundle: &mut ChangeGroup, repo: &RepoEntry, pr: &PullRequest) {
    let entry = PublicationEntry {
        repo_id: repo.id.clone(),
        provider: PROVIDER.to_string(),
        kind: PULL_REQUEST_KIND.to_string(),
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

    if let Some(existing) = bundle.publications.iter_mut().find(|publication| {
        publication.provider == PROVIDER
            && publication.kind == PULL_REQUEST_KIND
            && publication.repo_id == repo.id
    }) {
        *existing = entry;
    } else {
        bundle.publications.push(entry);
    }
    bundle.updated_at = now_iso();
}

pub fn find_existing_pr(
    cwd: &Path,
    branch: &str,
    base_branch: &str,
) -> Result<Option<PullRequest>> {
    let output = gh_output(
        cwd,
        [
            OsString::from("pr"),
            OsString::from("list"),
            OsString::from("--head"),
            OsString::from(branch),
            OsString::from("--base"),
            OsString::from(base_branch),
            OsString::from("--state"),
            OsString::from("all"),
            OsString::from("--json"),
            OsString::from(
                "number,url,state,title,baseRefName,headRefName,body,isDraft,headRefOid",
            ),
            OsString::from("--limit"),
            OsString::from("1"),
        ],
        None,
    )?;
    let prs: Vec<PullRequest> =
        serde_json::from_str(&output).context("failed to parse `gh pr list` JSON")?;
    Ok(prs.into_iter().next())
}

pub fn create_pr(
    cwd: &Path,
    base_branch: &str,
    head_branch: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<String> {
    let mut args = vec![
        OsString::from("pr"),
        OsString::from("create"),
        OsString::from("--base"),
        OsString::from(base_branch),
        OsString::from("--head"),
        OsString::from(head_branch),
        OsString::from("--title"),
        OsString::from(title),
        OsString::from("--body-file"),
        OsString::from("-"),
    ];
    if draft {
        args.push(OsString::from("--draft"));
    }

    let output = gh_output(cwd, args, Some(body))?;
    parse_pr_url(&output).context("`gh pr create` did not print a PR URL")
}

pub fn view_pr(cwd: &Path, url: &str) -> Result<PullRequest> {
    let output = gh_output(
        cwd,
        [
            OsString::from("pr"),
            OsString::from("view"),
            OsString::from(url),
            OsString::from("--json"),
            OsString::from(
                "number,url,state,title,baseRefName,headRefName,body,isDraft,headRefOid",
            ),
        ],
        None,
    )?;
    serde_json::from_str(&output).context("failed to parse `gh pr view` JSON")
}

pub fn edit_pr_body(cwd: &Path, url: &str, body: &str) -> Result<()> {
    gh_output(
        cwd,
        [
            OsString::from("pr"),
            OsString::from("edit"),
            OsString::from(url),
            OsString::from("--body-file"),
            OsString::from("-"),
        ],
        Some(body),
    )?;
    Ok(())
}

pub fn merge_pr(
    cwd: &Path,
    url: &str,
    method: &str,
    delete_branch: bool,
    match_head_sha: Option<&str>,
) -> Result<()> {
    let method_flag = match method {
        "merge" => "--merge",
        "rebase" => "--rebase",
        "squash" => "--squash",
        other => bail!("unknown GitHub merge method `{other}`"),
    };
    let mut args = vec![
        OsString::from("pr"),
        OsString::from("merge"),
        OsString::from(url),
        OsString::from(method_flag),
    ];
    if delete_branch {
        args.push(OsString::from("--delete-branch"));
    }
    if let Some(sha) = match_head_sha {
        args.push(OsString::from("--match-head-commit"));
        args.push(OsString::from(sha));
    }

    gh_output(cwd, args, None)?;
    Ok(())
}

pub fn check_runs(cwd: &Path, url: &str, required_only: bool) -> Result<Vec<CheckRun>> {
    let mut args = vec![
        OsString::from("pr"),
        OsString::from("checks"),
        OsString::from(url),
        OsString::from("--json"),
        OsString::from("name,state,bucket"),
    ];
    if required_only {
        args.push(OsString::from("--required"));
    }

    match gh_output(cwd, args, None) {
        Ok(output) if output.trim().is_empty() => Ok(Vec::new()),
        Ok(output) => serde_json::from_str(&output).context("failed to parse `gh pr checks` JSON"),
        Err(error) if error.to_string().to_lowercase().contains("no checks") => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub fn wait_for_checks(
    cwd: &Path,
    url: &str,
    required_only: bool,
    timeout_seconds: u64,
    interval_seconds: u64,
) -> Result<CheckWaitSummary> {
    let started = Instant::now();
    let timeout = Duration::from_secs(timeout_seconds);
    let interval = Duration::from_secs(interval_seconds.max(1));

    loop {
        let runs = check_runs(cwd, url, required_only)?;
        let state = checks_state(&runs);
        match state {
            ChecksState::NoChecks => {
                return Ok(CheckWaitSummary {
                    status: "no_required_checks".to_string(),
                    runs,
                })
            }
            ChecksState::Passed => {
                return Ok(CheckWaitSummary {
                    status: "passed".to_string(),
                    runs,
                })
            }
            ChecksState::Failed(name) => bail!("check `{name}` failed for {url}"),
            ChecksState::Pending => {
                if started.elapsed() >= timeout {
                    bail!("timed out waiting for checks on {url}");
                }
                thread::sleep(interval);
            }
        }
    }
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

fn gh_output<I, S>(cwd: &Path, args: I, stdin: Option<&str>) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<_>>();
    let mut child = Command::new("gh")
        .args(&args)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "failed to run `gh {}` in {}. Install and authenticate GitHub CLI to use Knit GitHub provider commands.",
                display_args(&args),
                cwd.display()
            )
        })?;

    if let Some(input) = stdin {
        let mut child_stdin = child
            .stdin
            .take()
            .context("failed to open stdin for GitHub CLI")?;
        child_stdin
            .write_all(input.as_bytes())
            .context("failed to write input to GitHub CLI")?;
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for `gh {}`", display_args(&args)))?;

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
        "gh {} failed in {}: {}",
        display_args(&args),
        cwd.display(),
        detail
    );
}

fn parse_pr_url(output: &str) -> Option<String> {
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
