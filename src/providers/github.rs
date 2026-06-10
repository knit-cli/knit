use super::{
    cli_output, parse_pr_url, pr_number_from_url, repo_scoped_args, CheckRun, Forge, PrTarget,
    PullRequest, PULL_REQUEST_KIND,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::ffi::OsString;

const CLI: &str = "gh";
const PR_JSON_FIELDS: &str =
    "number,url,state,title,baseRefName,headRefName,body,isDraft,headRefOid,mergeable,mergeStateStatus,reviewDecision";

/// GitHub forge adapter, backed by the `gh` CLI.
pub struct GitHub;

impl Forge for GitHub {
    fn id(&self) -> &'static str {
        "github"
    }

    fn review_kind(&self) -> &'static str {
        PULL_REQUEST_KIND
    }

    fn cli(&self) -> &'static str {
        CLI
    }

    fn repo_full_name(&self, remote: &str) -> Option<String> {
        full_name(remote)
    }

    fn find_existing(
        &self,
        target: &PrTarget,
        head: &str,
        base: &str,
    ) -> Result<Option<PullRequest>> {
        if let Some(repo_full_name) = &target.repo_full_name {
            return find_existing_with_api(target, repo_full_name, head, base);
        }

        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("pr"),
                OsString::from("list"),
                OsString::from("--head"),
                OsString::from(head),
                OsString::from("--base"),
                OsString::from(base),
                OsString::from("--state"),
                OsString::from("all"),
                OsString::from("--json"),
                OsString::from(PR_JSON_FIELDS),
                OsString::from("--limit"),
                OsString::from("1"),
            ],
        );
        let output = cli_output(CLI, &target.cwd, args, None)?;
        let prs: Vec<PullRequest> =
            serde_json::from_str(&output).context("failed to parse `gh pr list` JSON")?;
        Ok(prs.into_iter().next())
    }

    fn create(
        &self,
        target: &PrTarget,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<String> {
        if let Some(repo_full_name) = &target.repo_full_name {
            return create_with_api(target, repo_full_name, base, head, title, body, draft);
        }

        let mut args = vec![
            OsString::from("pr"),
            OsString::from("create"),
            OsString::from("--base"),
            OsString::from(base),
            OsString::from("--head"),
            OsString::from(head),
            OsString::from("--title"),
            OsString::from(title),
            OsString::from("--body-file"),
            OsString::from("-"),
        ];
        if draft {
            args.push(OsString::from("--draft"));
        }
        let args = repo_scoped_args(target, "--repo", args);
        let output = cli_output(CLI, &target.cwd, args, Some(body))?;
        parse_pr_url(&output).context("`gh pr create` did not print a PR URL")
    }

    fn view(&self, target: &PrTarget, selector: &str) -> Result<PullRequest> {
        if let Some(repo_full_name) = &target.repo_full_name {
            return view_with_api(target, repo_full_name, selector);
        }

        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("pr"),
                OsString::from("view"),
                OsString::from(selector),
                OsString::from("--json"),
                OsString::from(PR_JSON_FIELDS),
            ],
        );
        let output = cli_output(CLI, &target.cwd, args, None)?;
        serde_json::from_str(&output).context("failed to parse `gh pr view` JSON")
    }

    fn edit_body(&self, target: &PrTarget, selector: &str, body: &str) -> Result<()> {
        if let Some(repo_full_name) = &target.repo_full_name {
            return edit_body_with_api(target, repo_full_name, selector, body);
        }

        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("pr"),
                OsString::from("edit"),
                OsString::from(selector),
                OsString::from("--body-file"),
                OsString::from("-"),
            ],
        );
        cli_output(CLI, &target.cwd, args, Some(body))?;
        Ok(())
    }

    fn merge(
        &self,
        target: &PrTarget,
        selector: &str,
        method: &str,
        delete_branch: bool,
        match_head: Option<&str>,
    ) -> Result<()> {
        if let Some(repo_full_name) = &target.repo_full_name {
            if use_native_github_api(target) {
                return merge_with_api(
                    target,
                    repo_full_name,
                    selector,
                    method,
                    delete_branch,
                    match_head,
                );
            }
        }

        let method_flag = match method {
            "merge" => "--merge",
            "rebase" => "--rebase",
            "squash" => "--squash",
            other => bail!("unknown GitHub merge method `{other}`"),
        };
        let mut args = vec![
            OsString::from("pr"),
            OsString::from("merge"),
            OsString::from(selector),
            OsString::from(method_flag),
        ];
        if delete_branch {
            args.push(OsString::from("--delete-branch"));
        }
        if let Some(sha) = match_head {
            args.push(OsString::from("--match-head-commit"));
            args.push(OsString::from(sha));
        }
        let args = repo_scoped_args(target, "--repo", args);
        cli_output(CLI, &target.cwd, args, None)?;
        Ok(())
    }

    fn revert_pull_request(
        &self,
        target: &PrTarget,
        selector: &str,
        title: &str,
        body: &str,
    ) -> Result<String> {
        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("pr"),
                OsString::from("revert"),
                OsString::from(selector),
                OsString::from("--title"),
                OsString::from(title),
                OsString::from("--body-file"),
                OsString::from("-"),
            ],
        );
        let output = cli_output(CLI, &target.cwd, args, Some(body))?;
        parse_pr_url(&output).context("`gh pr revert` did not print a PR URL")
    }

    fn check_runs(
        &self,
        target: &PrTarget,
        selector: &str,
        required_only: bool,
    ) -> Result<Vec<CheckRun>> {
        if let Some(repo_full_name) = &target.repo_full_name {
            if use_native_github_api(target) {
                return check_runs_with_api(target, repo_full_name, selector, required_only);
            }
        }

        let mut args = vec![
            OsString::from("pr"),
            OsString::from("checks"),
            OsString::from(selector),
            OsString::from("--json"),
            OsString::from("name,state,bucket"),
        ];
        if required_only {
            args.push(OsString::from("--required"));
        }
        let args = repo_scoped_args(target, "--repo", args);

        match cli_output(CLI, &target.cwd, args, None) {
            Ok(output) if output.trim().is_empty() => Ok(Vec::new()),
            Ok(output) => {
                serde_json::from_str(&output).context("failed to parse `gh pr checks` JSON")
            }
            Err(error) if is_no_checks_error(&error) => Ok(Vec::new()),
            Err(error) if is_checks_permission_error(&error) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }
}

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

#[derive(Debug, Deserialize)]
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

impl Default for GitHubApiRef {
    fn default() -> Self {
        Self {
            ref_name: None,
            sha: None,
        }
    }
}

fn find_existing_with_api(
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

fn create_with_api(
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

fn view_with_api(target: &PrTarget, repo_full_name: &str, selector: &str) -> Result<PullRequest> {
    let number = selector_pr_number(selector)
        .with_context(|| format!("could not determine GitHub PR number from `{selector}`"))?;
    let endpoint = pull_request_api_item_endpoint(repo_full_name, number);
    let output = github_api_output(target, "GET", &endpoint, None)?;
    let pr: GitHubApiPullRequest =
        serde_json::from_str(&output).context("failed to parse GitHub pull API JSON")?;
    Ok(pr.into_pull_request())
}

fn edit_body_with_api(
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

fn merge_with_api(
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
        Some(view_with_api(target, repo_full_name, selector)?)
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

fn check_runs_with_api(
    target: &PrTarget,
    repo_full_name: &str,
    selector: &str,
    _required_only: bool,
) -> Result<Vec<CheckRun>> {
    let pr = view_with_api(target, repo_full_name, selector)?;
    let sha = pr
        .head_ref_oid
        .as_deref()
        .filter(|sha| !sha.is_empty())
        .with_context(|| format!("could not determine head SHA for GitHub PR `{selector}`"))?;

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

fn github_api_output(
    target: &PrTarget,
    method: &str,
    endpoint: &str,
    body: Option<&str>,
) -> Result<String> {
    if use_native_github_api(target) {
        return native_github_api_output(method, endpoint, body);
    }

    let mut args = vec![OsString::from("api")];
    if method != "GET" {
        args.push(OsString::from("--method"));
        args.push(OsString::from(method));
    }
    args.push(OsString::from(endpoint));
    if body.is_some() {
        args.push(OsString::from("--input"));
        args.push(OsString::from("-"));
    }
    cli_output(CLI, &target.cwd, args, body)
}

fn use_native_github_api(target: &PrTarget) -> bool {
    target.repo_full_name.is_some()
        && std::env::var("KNIT_GITHUB_API_TRANSPORT")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    // "curl"/"curl-ipv4"/"ipv4" are the historical values from
                    // when this transport shelled out to `curl --ipv4`; they
                    // keep selecting the same (now native) IPv4-first transport.
                    "curl" | "curl-ipv4" | "ipv4" | "native" | "api"
                )
            })
            .unwrap_or(false)
}

fn github_api_base() -> String {
    std::env::var("KNIT_GITHUB_API_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
}

/// Resolve hostnames preferring IPv4 addresses. This transport exists for
/// non-interactive runtimes where default IPv6 routing can hang simple GitHub
/// I/O, so v6 addresses are only used when no v4 address resolves.
fn ipv4_first_resolver(netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    let all: Vec<std::net::SocketAddr> = netloc.to_socket_addrs()?.collect();
    let v4: Vec<std::net::SocketAddr> = all
        .iter()
        .copied()
        .filter(std::net::SocketAddr::is_ipv4)
        .collect();
    Ok(if v4.is_empty() { all } else { v4 })
}

fn native_github_api_output(method: &str, endpoint: &str, body: Option<&str>) -> Result<String> {
    let token = github_api_token()
        .context("KNIT_GITHUB_API_TRANSPORT requires GH_TOKEN or GITHUB_TOKEN")?;
    let url = format!("{}/{}", github_api_base(), endpoint.trim_start_matches('/'));
    let operation = format!("{method} /{}", endpoint.trim_start_matches('/'));

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(20))
        .resolver(
            ipv4_first_resolver as fn(&str) -> std::io::Result<Vec<std::net::SocketAddr>>,
        )
        .build();
    let mut request = agent
        .request(method, &url)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "knit")
        .set("Authorization", &format!("Bearer {token}"));
    if body.is_some() {
        request = request.set("Content-Type", "application/json");
    }
    let result = match body {
        Some(input) => request.send_string(input),
        None => request.call(),
    };

    match result {
        Ok(response) => {
            let text = response
                .into_string()
                .with_context(|| format!("failed to read GitHub API response for {operation}"))?;
            Ok(text.trim_end().to_string())
        }
        Err(ureq::Error::Status(status, response)) => {
            let detail = response.into_string().unwrap_or_default();
            let detail = detail.trim();
            if status == 401 || looks_like_github_auth_failure(detail) {
                bail!(
                    "GitHub API request failed during {operation}: HTTP {status}: {detail}\nHint: GitHub rejected GH_TOKEN/GITHUB_TOKEN. Replace the saved GitHub credential with an active token that can access this repository, then retry."
                );
            }
            bail!("GitHub API request failed during {operation}: HTTP {status}: {detail}");
        }
        Err(ureq::Error::Transport(transport)) => {
            bail!("GitHub API request failed during {operation}: {transport}")
        }
    }
}

fn github_api_token() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"].into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn looks_like_github_auth_failure(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("bad credentials") || lower.contains("unauthorized")
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

/// Parse `owner/name` from a GitHub remote URL.
pub(crate) fn full_name(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches(".git");
    let marker = "github.com";
    let index = remote.find(marker)?;
    let suffix = remote[index + marker.len()..].trim_start_matches([':', '/']);
    let (owner, name) = suffix.split_once('/')?;
    let name = name.split('/').next().unwrap_or(name);
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

fn is_no_checks_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("no checks")
        || message.contains("no required checks")
        || message.contains("no required status checks")
        || message.contains("no check runs")
        || message.contains("no check suites")
        || message.contains("no checks reported")
        || message.contains("no required checks reported")
}

fn is_checks_permission_error(error: &anyhow::Error) -> bool {
    super::is_gh_checks_access_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn treats_checks_permission_errors_as_nonfatal() {
        let err = anyhow::anyhow!(
            "gh pr checks https://github.com/marc-merino/betsnitch-frontend/pull/45 --json name,state,bucket --required --repo Marc-Merino/betsnitch-frontend failed in /tmp: GraphQL: Resource not accessible by personal access token (node.statusCheckRollup.nodes.0.commit.statusCheckRollup)"
        );
        assert!(is_checks_permission_error(&err));
    }

    #[test]
    fn parses_full_name_from_remote_forms() {
        assert_eq!(
            full_name("https://github.com/acme/backend.git").as_deref(),
            Some("acme/backend")
        );
        assert_eq!(
            full_name("git@github.com:acme/backend.git").as_deref(),
            Some("acme/backend")
        );
        assert_eq!(
            full_name("https://example.com/acme/backend").as_deref(),
            None
        );
    }

    #[test]
    fn parses_pr_view_json() {
        let json = r#"{"number":7,"url":"https://github.com/acme/backend/pull/7","state":"OPEN","title":"t","baseRefName":"main","headRefName":"knit/x","isDraft":false,"headRefOid":"abc"}"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(pr.base_ref_name.as_deref(), Some("main"));
        assert_eq!(pr.head_ref_oid.as_deref(), Some("abc"));
    }

    #[test]
    fn pr_number_parsed_from_url() {
        assert_eq!(
            super::super::pr_number_from_url("https://github.com/acme/backend/pull/42"),
            Some(42)
        );
    }

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

    #[test]
    fn github_auth_failure_detects_bad_credentials() {
        assert!(looks_like_github_auth_failure(
            "{\"message\":\"Bad credentials\"}"
        ));
        assert!(!looks_like_github_auth_failure(
            "{\"message\":\"Not Found\"}"
        ));
    }

    #[test]
    fn ipv4_first_resolver_prefers_v4_addresses() {
        let addrs = ipv4_first_resolver("localhost:80").unwrap();
        assert!(!addrs.is_empty());
        // When any IPv4 address resolves, only IPv4 addresses are returned.
        if addrs.iter().any(std::net::SocketAddr::is_ipv4) {
            assert!(addrs.iter().all(std::net::SocketAddr::is_ipv4));
        }
    }
}
