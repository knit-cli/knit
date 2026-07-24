//! GitHub REST API operations, used whenever the target carries a repo full
//! name. Each operation goes through [`transport::github_api_output`], which
//! picks between `gh api` and the native HTTP client.

use super::transport::{github_api_output, native_github_api_output, use_native_github_api};
use super::CLI;
use crate::providers::{
    cli_output, parse_pr_url, pr_number_from_url, CheckRun, PrTarget, PullRequest,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::ffi::OsString;

#[derive(Debug, Deserialize)]
struct GitHubApiPullRequest {
    number: u64,
    html_url: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    head: Option<GitHubApiRef>,
    #[serde(default)]
    base: Option<GitHubApiRef>,
    #[serde(default)]
    merged: Option<bool>,
    #[serde(default)]
    mergeable: Option<bool>,
    #[serde(default)]
    mergeable_state: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GitHubApiRef {
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    #[serde(default)]
    sha: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubApiCheckRunCollection {
    #[serde(default)]
    check_runs: Vec<GitHubApiCheckRun>,
}

#[derive(Debug, Deserialize)]
struct GitHubApiCheckRun {
    name: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubApiStatusCollection {
    #[serde(default)]
    statuses: Vec<GitHubApiStatus>,
}

#[derive(Debug, Deserialize)]
struct GitHubApiStatus {
    #[serde(default)]
    context: Option<String>,
    state: String,
}

impl GitHubApiPullRequest {
    fn into_pull_request(self) -> PullRequest {
        let head = self.head.unwrap_or_default();
        let base = self.base.unwrap_or_default();
        let merged = self.merged.unwrap_or(false);
        PullRequest {
            number: self.number,
            url: self.html_url,
            state: github_api_state(self.state.as_deref(), merged),
            title: self.title,
            base_ref_name: base.ref_name,
            head_ref_name: head.ref_name,
            body: self.body,
            is_draft: self.draft,
            head_ref_oid: head.sha,
            mergeable: github_api_mergeable(self.mergeable, self.mergeable_state.as_deref()),
            merge_state_status: self.mergeable_state.map(|state| state.to_ascii_uppercase()),
            review_decision: None,
        }
    }
}

pub(super) fn find_existing(
    target: &PrTarget,
    repo_full_name: &str,
    head: &str,
    base: &str,
) -> Result<Option<PullRequest>> {
    let endpoint = pull_request_search_api_endpoint(repo_full_name, head, base)?;
    let output = github_api_output(target, "GET", &endpoint, None)?;
    let prs: Vec<GitHubApiPullRequest> =
        serde_json::from_str(&output).context("failed to parse GitHub pulls API JSON")?;
    Ok(prs
        .into_iter()
        .next()
        .map(GitHubApiPullRequest::into_pull_request))
}

pub(super) fn create(
    target: &PrTarget,
    repo_full_name: &str,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<String> {
    let payload = create_pull_request_payload(base, head, title, body, draft)?;
    if use_native_github_api(target) {
        let output = native_github_api_output(
            "POST",
            &pull_request_api_endpoint(repo_full_name),
            Some(&payload),
        )?;
        let pr: GitHubApiPullRequest =
            serde_json::from_str(&output).context("failed to parse GitHub pull create API JSON")?;
        return Ok(pr.html_url);
    }

    let args = vec![
        OsString::from("api"),
        OsString::from("--method"),
        OsString::from("POST"),
        OsString::from(pull_request_api_endpoint(repo_full_name)),
        OsString::from("--input"),
        OsString::from("-"),
        OsString::from("--jq"),
        OsString::from(".html_url"),
    ];
    let output = cli_output(CLI, &target.cwd, args, Some(&payload))?;
    parse_pr_url(&output)
        .or_else(|| {
            let trimmed = output.trim();
            trimmed.starts_with("http").then(|| trimmed.to_string())
        })
        .context("`gh api` did not return a PR URL")
}

pub(super) fn view(target: &PrTarget, repo_full_name: &str, selector: &str) -> Result<PullRequest> {
    let number = selector_pr_number(selector)
        .with_context(|| format!("could not determine GitHub PR number from `{selector}`"))?;
    let endpoint = pull_request_api_item_endpoint(repo_full_name, number);
    let output = github_api_output(target, "GET", &endpoint, None)?;
    let pr: GitHubApiPullRequest =
        serde_json::from_str(&output).context("failed to parse GitHub pull API JSON")?;
    Ok(pr.into_pull_request())
}

pub(super) fn edit_body(
    target: &PrTarget,
    repo_full_name: &str,
    selector: &str,
    body: &str,
) -> Result<()> {
    let number = selector_pr_number(selector)
        .with_context(|| format!("could not determine GitHub PR number from `{selector}`"))?;
    let payload = serde_json::to_string(&json!({ "body": body }))
        .context("failed to encode GitHub pull request edit payload")?;
    let endpoint = pull_request_api_item_endpoint(repo_full_name, number);
    github_api_output(target, "PATCH", &endpoint, Some(&payload))?;
    Ok(())
}

pub(super) fn edit_base(
    target: &PrTarget,
    repo_full_name: &str,
    selector: &str,
    base: &str,
) -> Result<()> {
    let number = selector_pr_number(selector)
        .with_context(|| format!("could not determine GitHub PR number from `{selector}`"))?;
    let payload = serde_json::to_string(&json!({ "base": base }))
        .context("failed to encode GitHub pull request target payload")?;
    let endpoint = pull_request_api_item_endpoint(repo_full_name, number);
    github_api_output(target, "PATCH", &endpoint, Some(&payload))?;
    Ok(())
}

pub(super) fn merge(
    target: &PrTarget,
    repo_full_name: &str,
    selector: &str,
    method: &str,
    delete_branch: bool,
    match_head: Option<&str>,
) -> Result<()> {
    if !matches!(method, "merge" | "rebase" | "squash") {
        bail!("unknown GitHub merge method `{method}`");
    }
    let number = selector_pr_number(selector)
        .with_context(|| format!("could not determine GitHub PR number from `{selector}`"))?;
    let pr_before_merge = if delete_branch {
        Some(view(target, repo_full_name, selector)?)
    } else {
        None
    };

    let mut payload = json!({ "merge_method": method });
    if let Some(sha) = match_head {
        payload["sha"] = json!(sha);
    }
    let payload = serde_json::to_string(&payload)
        .context("failed to encode GitHub pull request merge payload")?;
    let endpoint = format!(
        "{}/{number}/merge",
        pull_request_api_endpoint(repo_full_name)
    );
    github_api_output(target, "PUT", &endpoint, Some(&payload))?;

    if let Some(pr) = pr_before_merge {
        if let Some(branch) = pr
            .head_ref_name
            .as_deref()
            .filter(|branch| !branch.is_empty())
        {
            let endpoint = git_ref_api_endpoint(repo_full_name, "heads", branch);
            github_api_output(target, "DELETE", &endpoint, None)
                .with_context(|| format!("failed to delete GitHub branch `{branch}`"))?;
        }
    }

    Ok(())
}

pub(super) fn check_runs(
    target: &PrTarget,
    repo_full_name: &str,
    selector: &str,
    _required_only: bool,
) -> Result<Vec<CheckRun>> {
    let pr = view(target, repo_full_name, selector)?;
    let sha = pr
        .head_ref_oid
        .as_deref()
        .filter(|sha| !sha.is_empty())
        .with_context(|| format!("could not determine head SHA for GitHub PR `{selector}`"))?;

    commit_check_runs(target, repo_full_name, sha)
}

pub(super) fn commit_check_runs(
    target: &PrTarget,
    repo_full_name: &str,
    sha: &str,
) -> Result<Vec<CheckRun>> {
    let mut runs = Vec::new();

    let check_runs_endpoint = format!(
        "repos/{repo_full_name}/commits/{}/check-runs?per_page=100",
        encode_path_component(sha)
    );
    let output = github_api_output(target, "GET", &check_runs_endpoint, None)?;
    let collection: GitHubApiCheckRunCollection =
        serde_json::from_str(&output).context("failed to parse GitHub check-runs API JSON")?;
    runs.extend(collection.check_runs.into_iter().map(Into::into));

    let status_endpoint = format!(
        "repos/{repo_full_name}/commits/{}/status",
        encode_path_component(sha)
    );
    let output = github_api_output(target, "GET", &status_endpoint, None)?;
    let collection: GitHubApiStatusCollection =
        serde_json::from_str(&output).context("failed to parse GitHub statuses API JSON")?;
    runs.extend(collection.statuses.into_iter().map(Into::into));

    Ok(runs)
}

fn pull_request_api_endpoint(repo_full_name: &str) -> String {
    format!("repos/{repo_full_name}/pulls")
}

fn pull_request_api_item_endpoint(repo_full_name: &str, number: u64) -> String {
    format!("{}/{number}", pull_request_api_endpoint(repo_full_name))
}

fn git_ref_api_endpoint(repo_full_name: &str, namespace: &str, name: &str) -> String {
    format!(
        "repos/{repo_full_name}/git/refs/{}/{}",
        encode_path_component(namespace),
        encode_path_allow_slash(name)
    )
}

fn pull_request_search_api_endpoint(
    repo_full_name: &str,
    head: &str,
    base: &str,
) -> Result<String> {
    let head_owner = repo_owner(repo_full_name)?;
    let head = if head.contains(':') {
        head.to_string()
    } else {
        format!("{head_owner}:{head}")
    };
    Ok(format!(
        "{}?state=all&head={}&base={}&per_page=1",
        pull_request_api_endpoint(repo_full_name),
        encode_query_component(&head),
        encode_query_component(base)
    ))
}

fn create_pull_request_payload(
    base: &str,
    head: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<String> {
    serde_json::to_string(&json!({
        "base": base,
        "head": head,
        "title": title,
        "body": body,
        "draft": draft,
    }))
    .context("failed to encode GitHub pull request payload")
}

fn repo_owner(repo_full_name: &str) -> Result<&str> {
    repo_full_name
        .split_once('/')
        .map(|(owner, _repo)| owner)
        .filter(|owner| !owner.is_empty())
        .with_context(|| format!("invalid GitHub repository name `{repo_full_name}`"))
}

fn selector_pr_number(selector: &str) -> Option<u64> {
    selector
        .trim()
        .parse()
        .ok()
        .or_else(|| pr_number_from_url(selector))
}

fn github_api_state(state: Option<&str>, merged: bool) -> Option<String> {
    if merged {
        return Some("MERGED".to_string());
    }
    state.map(|state| state.to_ascii_uppercase())
}

fn github_api_mergeable(mergeable: Option<bool>, mergeable_state: Option<&str>) -> Option<String> {
    match (mergeable, mergeable_state) {
        (Some(true), _) => Some("MERGEABLE".to_string()),
        (Some(false), Some("dirty" | "DIRTY")) => Some("CONFLICTING".to_string()),
        (Some(false), _) => Some("CONFLICTING".to_string()),
        (None, _) => None,
    }
}

impl From<GitHubApiCheckRun> for CheckRun {
    fn from(run: GitHubApiCheckRun) -> Self {
        let conclusion = run.conclusion.as_deref().unwrap_or("");
        let status = run.status.as_deref().unwrap_or("");
        let (state, bucket) = match (status, conclusion) {
            (_, "success") => (Some("SUCCESS".to_string()), Some("pass".to_string())),
            (_, "skipped" | "neutral") => {
                (Some("SKIPPED".to_string()), Some("skipping".to_string()))
            }
            (_, "failure" | "timed_out" | "action_required") => {
                (Some("FAILURE".to_string()), Some("fail".to_string()))
            }
            (_, "cancelled") => (Some("CANCELLED".to_string()), Some("cancel".to_string())),
            ("completed", _) => (Some("FAILURE".to_string()), Some("fail".to_string())),
            _ => (run.status.map(|state| state.to_ascii_uppercase()), None),
        };
        CheckRun {
            name: run.name,
            state,
            bucket,
        }
    }
}

impl From<GitHubApiStatus> for CheckRun {
    fn from(status: GitHubApiStatus) -> Self {
        let (state, bucket) = match status.state.as_str() {
            "success" => (Some("SUCCESS".to_string()), Some("pass".to_string())),
            "failure" | "error" => (Some("FAILURE".to_string()), Some("fail".to_string())),
            _ => (Some(status.state.to_ascii_uppercase()), None),
        };
        CheckRun {
            name: status.context.unwrap_or_else(|| "status".to_string()),
            state,
            bucket,
        }
    }
}

fn encode_query_component(input: &str) -> String {
    let mut encoded = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                use std::fmt::Write as _;
                write!(&mut encoded, "%{byte:02X}").expect("writing to a string cannot fail");
            }
        }
    }
    encoded
}

fn encode_path_component(input: &str) -> String {
    encode_path(input, false)
}

fn encode_path_allow_slash(input: &str) -> String {
    encode_path(input, true)
}

fn encode_path(input: &str, allow_slash: bool) -> String {
    let mut encoded = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            b'/' if allow_slash => encoded.push('/'),
            _ => {
                use std::fmt::Write as _;
                write!(&mut encoded, "%{byte:02X}").expect("writing to a string cannot fail");
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_create_uses_repo_scoped_api_payload() {
        assert_eq!(
            pull_request_api_endpoint("acme/backend"),
            "repos/acme/backend/pulls"
        );
        assert_eq!(
            pull_request_search_api_endpoint("acme/backend", "knit/testbun", "main").unwrap(),
            "repos/acme/backend/pulls?state=all&head=acme%3Aknit%2Ftestbun&base=main&per_page=1"
        );

        let payload = create_pull_request_payload(
            "main",
            "knit/testbun",
            "feature title",
            "Body line one\nBody line two",
            true,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();

        assert_eq!(value["base"], "main");
        assert_eq!(value["head"], "knit/testbun");
        assert_eq!(value["title"], "feature title");
        assert_eq!(value["body"], "Body line one\nBody line two");
        assert_eq!(value["draft"], true);
    }
}
