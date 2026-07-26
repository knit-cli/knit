use super::{
    cli_output, parse_pr_url, repo_scoped_args, CheckRun, Forge, PrTarget, PullRequest,
    MERGE_REQUEST_KIND,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::ffi::OsString;
use std::time::{SystemTime, UNIX_EPOCH};

const CLI: &str = "glab";

/// GitLab forge adapter, backed by the `glab` CLI. Review objects are merge requests.
pub struct GitLab;

#[derive(Debug, Deserialize)]
struct GlabMr {
    iid: u64,
    web_url: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    target_branch: Option<String>,
    #[serde(default)]
    source_branch: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    work_in_progress: Option<bool>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    detailed_merge_status: Option<String>,
    #[serde(default)]
    merge_status: Option<String>,
    #[serde(default)]
    merge_commit_sha: Option<String>,
    #[serde(default)]
    head_pipeline: Option<GlabPipeline>,
    #[serde(default)]
    pipeline: Option<GlabPipeline>,
}

#[derive(Debug, Deserialize)]
struct GlabPipeline {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitLabApprovals {
    #[serde(default)]
    approved: bool,
}

#[derive(Debug, Deserialize)]
struct GitLabJob {
    #[serde(default)]
    name: Option<String>,
    status: String,
}

impl Forge for GitLab {
    fn id(&self) -> &'static str {
        "gitlab"
    }

    fn review_kind(&self) -> &'static str {
        MERGE_REQUEST_KIND
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
        if target.repo_full_name.is_some() {
            let repo = resolve_repo(target)?;
            let endpoint = format!(
                "projects/{}/merge_requests?scope=all&source_branch={}&target_branch={}&per_page=1",
                encode_path_component(&repo),
                encode_query_component(head),
                encode_query_component(base)
            );
            let output = api_output(target, "GET", &endpoint, None)?;
            let mrs: Vec<GlabMr> =
                serde_json::from_str(&output).context("failed to parse GitLab MR list JSON")?;
            return mrs
                .into_iter()
                .next()
                .map(|mr| enrich_pull_request(target, &repo, mr))
                .transpose();
        }
        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("mr"),
                OsString::from("list"),
                OsString::from("--source-branch"),
                OsString::from(head),
                OsString::from("--target-branch"),
                OsString::from(base),
                OsString::from("--all"),
                OsString::from("--output"),
                OsString::from("json"),
                OsString::from("--per-page"),
                OsString::from("1"),
            ],
        );
        let output = cli_output(CLI, &target.cwd, args, None)?;
        if output.trim().is_empty() {
            return Ok(None);
        }
        let mrs: Vec<GlabMr> =
            serde_json::from_str(&output).context("failed to parse `glab mr list` JSON")?;
        let repo = resolve_repo(target)?;
        mrs.into_iter()
            .next()
            .map(|mr| enrich_pull_request(target, &repo, mr))
            .transpose()
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
        if target.repo_full_name.is_some() {
            let repo = resolve_repo(target)?;
            let title = if draft && !title.starts_with("Draft:") {
                format!("Draft: {title}")
            } else {
                title.to_string()
            };
            let payload = serde_json::to_string(&json!({
                "source_branch": head,
                "target_branch": base,
                "title": title,
                "description": body,
            }))
            .context("failed to encode GitLab merge request payload")?;
            let output = api_output(
                target,
                "POST",
                &format!("projects/{}/merge_requests", encode_path_component(&repo)),
                Some(&payload),
            )?;
            let mr: GlabMr =
                serde_json::from_str(&output).context("failed to parse GitLab MR create JSON")?;
            return Ok(mr.web_url);
        }
        let mut args = vec![
            OsString::from("mr"),
            OsString::from("create"),
            OsString::from("--source-branch"),
            OsString::from(head),
            OsString::from("--target-branch"),
            OsString::from(base),
            OsString::from("--title"),
            OsString::from(title),
            OsString::from("--description"),
            OsString::from(body),
            OsString::from("--yes"),
        ];
        if draft {
            args.push(OsString::from("--draft"));
        }
        let args = repo_scoped_args(target, "--repo", args);
        let output = cli_output(CLI, &target.cwd, args, None)?;
        parse_pr_url(&output).context("`glab mr create` did not print an MR URL")
    }

    fn view(&self, target: &PrTarget, selector: &str) -> Result<PullRequest> {
        let repo = resolve_repo(target)?;
        if target.repo_full_name.is_some() {
            let output = api_output(
                target,
                "GET",
                &format!(
                    "projects/{}/merge_requests/{}",
                    encode_path_component(&repo),
                    selector_iid(selector)
                ),
                None,
            )?;
            let mr: GlabMr =
                serde_json::from_str(&output).context("failed to parse GitLab MR API JSON")?;
            return enrich_pull_request(target, &repo, mr);
        }
        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("mr"),
                OsString::from("view"),
                OsString::from(selector_iid(selector)),
                OsString::from("--output"),
                OsString::from("json"),
            ],
        );
        let output = cli_output(CLI, &target.cwd, args, None)?;
        let mr: GlabMr =
            serde_json::from_str(&output).context("failed to parse `glab mr view` JSON")?;
        enrich_pull_request(target, &repo, mr)
    }

    fn edit_body(&self, target: &PrTarget, selector: &str, body: &str) -> Result<()> {
        if target.repo_full_name.is_some() {
            return edit_merge_request(target, selector, &json!({ "description": body }), "body");
        }
        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("mr"),
                OsString::from("update"),
                OsString::from(selector_iid(selector)),
                OsString::from("--description"),
                OsString::from(body),
            ],
        );
        cli_output(CLI, &target.cwd, args, None)?;
        Ok(())
    }

    fn edit_base(&self, target: &PrTarget, selector: &str, base: &str) -> Result<()> {
        if target.repo_full_name.is_some() {
            return edit_merge_request(
                target,
                selector,
                &json!({ "target_branch": base }),
                "target",
            );
        }
        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("mr"),
                OsString::from("update"),
                OsString::from(selector_iid(selector)),
                OsString::from("--target-branch"),
                OsString::from(base),
            ],
        );
        cli_output(CLI, &target.cwd, args, None)?;
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
        if let Some(expected) = match_head.filter(|sha| !sha.is_empty()) {
            if let Some(actual) = self.view(target, selector)?.head_ref_oid {
                if !sha_matches(expected, &actual) {
                    bail!(
                        "GitLab MR {selector} head `{actual}` does not match expected `{expected}`; refusing to merge."
                    );
                }
            }
        }
        if target.repo_full_name.is_some() {
            let repo = resolve_repo(target)?;
            if method == "rebase" {
                api_output(
                    target,
                    "PUT",
                    &format!(
                        "projects/{}/merge_requests/{}/rebase",
                        encode_path_component(&repo),
                        selector_iid(selector)
                    ),
                    None,
                )?;
            } else if !matches!(method, "merge" | "squash") {
                bail!("unknown GitLab merge method `{method}`");
            }
            let mut payload = json!({
                "should_remove_source_branch": delete_branch,
                "squash": method == "squash",
            });
            if let Some(sha) = match_head {
                payload["sha"] = json!(sha);
            }
            let payload =
                serde_json::to_string(&payload).context("failed to encode GitLab merge payload")?;
            api_output(
                target,
                "PUT",
                &format!(
                    "projects/{}/merge_requests/{}/merge",
                    encode_path_component(&repo),
                    selector_iid(selector)
                ),
                Some(&payload),
            )?;
            return Ok(());
        }
        let mut args = vec![
            OsString::from("mr"),
            OsString::from("merge"),
            OsString::from(selector_iid(selector)),
            OsString::from("--yes"),
        ];
        match method {
            "merge" => {}
            "squash" => args.push(OsString::from("--squash")),
            "rebase" => args.push(OsString::from("--rebase")),
            other => bail!("unknown GitLab merge method `{other}`"),
        }
        if delete_branch {
            args.push(OsString::from("--remove-source-branch"));
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
        let repo = resolve_repo(target)?;
        let iid = selector_iid(selector);
        let mr = api_merge_request(target, &repo, &iid)?;
        let commit = mr
            .merge_commit_sha
            .as_deref()
            .filter(|sha| !sha.is_empty())
            .with_context(|| format!("GitLab MR {selector} has no merge commit to revert"))?;
        let target_branch = mr
            .target_branch
            .as_deref()
            .filter(|branch| !branch.is_empty())
            .context("GitLab MR has no target branch")?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let branch = format!("knit/revert-{}-{timestamp}", mr.iid);
        let payload = serde_json::to_string(&json!({
            "branch": branch,
            "ref": target_branch,
        }))
        .context("failed to encode GitLab revert branch payload")?;
        api_output(
            target,
            "POST",
            &format!(
                "projects/{}/repository/branches",
                encode_path_component(&repo)
            ),
            Some(&payload),
        )?;
        let payload = serde_json::to_string(&json!({ "branch": branch }))
            .context("failed to encode GitLab commit revert payload")?;
        api_output(
            target,
            "POST",
            &format!(
                "projects/{}/repository/commits/{}/revert",
                encode_path_component(&repo),
                encode_path_component(commit)
            ),
            Some(&payload),
        )?;
        let payload = serde_json::to_string(&json!({
            "source_branch": branch,
            "target_branch": target_branch,
            "title": title,
            "description": body,
        }))
        .context("failed to encode GitLab revert MR payload")?;
        let output = api_output(
            target,
            "POST",
            &format!("projects/{}/merge_requests", encode_path_component(&repo)),
            Some(&payload),
        )?;
        let revert: GlabMr =
            serde_json::from_str(&output).context("failed to parse GitLab revert MR JSON")?;
        Ok(revert.web_url)
    }

    fn check_runs(
        &self,
        target: &PrTarget,
        selector: &str,
        _required_only: bool,
    ) -> Result<Vec<CheckRun>> {
        let repo = resolve_repo(target)?;
        let iid = selector_iid(selector);
        let endpoint = format!(
            "projects/{}/merge_requests/{iid}/pipelines?per_page=1",
            encode_path_component(&repo)
        );
        if let Ok(output) = api_output(target, "GET", &endpoint, None) {
            let pipelines: Vec<GlabPipeline> = serde_json::from_str(&output)
                .context("failed to parse GitLab MR pipelines JSON")?;
            if let Some(pipeline) = pipelines.into_iter().next() {
                if let Some(id) = pipeline.id {
                    let output = api_output(
                        target,
                        "GET",
                        &format!(
                            "projects/{}/pipelines/{id}/jobs?per_page=100",
                            encode_path_component(&repo)
                        ),
                        None,
                    )?;
                    let jobs: Vec<GitLabJob> = serde_json::from_str(&output)
                        .context("failed to parse GitLab pipeline jobs JSON")?;
                    return Ok(jobs.into_iter().map(Into::into).collect());
                }
                return Ok(pipeline_check(Some(pipeline)));
            }
            return Ok(Vec::new());
        }

        // Older `glab` versions or tokens without pipeline API access still
        // provide a useful single synthetic pipeline result.
        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("mr"),
                OsString::from("view"),
                OsString::from(&iid),
                OsString::from("--output"),
                OsString::from("json"),
            ],
        );
        let output = cli_output(CLI, &target.cwd, args, None)?;
        let mr: GlabMr =
            serde_json::from_str(&output).context("failed to parse `glab mr view` JSON")?;
        Ok(pipeline_check(mr.head_pipeline.or(mr.pipeline)))
    }
}

fn resolve_repo(target: &PrTarget) -> Result<String> {
    if let Some(repo) = target
        .repo_full_name
        .as_deref()
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
    {
        return Ok(repo.to_string());
    }
    let remote = crate::git::git_output_optional(&target.cwd, ["remote", "get-url", "origin"])?
        .with_context(|| {
            format!(
                "could not resolve GitLab repository: no origin remote in {}",
                target.cwd.display()
            )
        })?;
    full_name(&remote).with_context(|| {
        format!(
            "could not parse a GitLab project path from origin `{}`",
            remote.trim()
        )
    })
}

fn api_merge_request(target: &PrTarget, repo: &str, iid: &str) -> Result<GlabMr> {
    let output = api_output(
        target,
        "GET",
        &format!(
            "projects/{}/merge_requests/{iid}",
            encode_path_component(repo)
        ),
        None,
    )?;
    serde_json::from_str(&output).context("failed to parse GitLab MR API JSON")
}

fn enrich_pull_request(target: &PrTarget, repo: &str, mr: GlabMr) -> Result<PullRequest> {
    let iid = mr.iid.to_string();
    let approval = api_output(
        target,
        "GET",
        &format!(
            "projects/{}/merge_requests/{iid}/approvals",
            encode_path_component(repo)
        ),
        None,
    )
    .ok()
    .and_then(|output| serde_json::from_str::<GitLabApprovals>(&output).ok())
    .is_some_and(|approval| approval.approved);
    let mut pr = into_pull_request(mr);
    if approval {
        pr.review_decision = Some("APPROVED".to_string());
    }
    Ok(pr)
}

fn edit_merge_request(
    target: &PrTarget,
    selector: &str,
    value: &serde_json::Value,
    label: &str,
) -> Result<()> {
    let repo = resolve_repo(target)?;
    let payload = serde_json::to_string(value)
        .with_context(|| format!("failed to encode GitLab MR {label} payload"))?;
    api_output(
        target,
        "PUT",
        &format!(
            "projects/{}/merge_requests/{}",
            encode_path_component(&repo),
            selector_iid(selector)
        ),
        Some(&payload),
    )?;
    Ok(())
}

pub(crate) fn commit_check_runs(target: &PrTarget, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
    let output = api_output(
        target,
        "GET",
        &format!(
            "projects/{}/repository/commits/{}/statuses?per_page=100",
            encode_path_component(repo),
            encode_path_component(sha)
        ),
        None,
    )?;
    let statuses: Vec<GitLabJob> =
        serde_json::from_str(&output).context("failed to parse GitLab commit statuses JSON")?;
    Ok(statuses.into_iter().map(Into::into).collect())
}

impl From<GitLabJob> for CheckRun {
    fn from(job: GitLabJob) -> Self {
        let (state, bucket) = gitlab_status_bucket(&job.status);
        CheckRun {
            name: job.name.unwrap_or_else(|| "job".to_string()),
            state: Some(state.to_string()),
            bucket: Some(bucket.to_string()),
        }
    }
}

fn gitlab_status_bucket(status: &str) -> (&'static str, &'static str) {
    match status.to_ascii_lowercase().as_str() {
        "success" => ("SUCCESS", "pass"),
        "failed" => ("FAILURE", "fail"),
        "canceled" | "cancelled" => ("CANCELLED", "cancel"),
        "skipped" | "manual" => ("SKIPPED", "skipping"),
        _ => ("RUNNING", "pending"),
    }
}

fn sha_matches(expected: &str, actual: &str) -> bool {
    expected.starts_with(actual) || actual.starts_with(expected)
}

fn api_output(
    target: &PrTarget,
    method: &str,
    endpoint: &str,
    body: Option<&str>,
) -> Result<String> {
    if target.repo_full_name.is_some() {
        return native_api_output(method, endpoint, body);
    }
    let mut args = vec![
        OsString::from("api"),
        OsString::from("--method"),
        OsString::from(method),
        OsString::from(endpoint),
    ];
    if body.is_some() {
        args.push(OsString::from("--input"));
        args.push(OsString::from("-"));
    }
    cli_output(CLI, &target.cwd, args, body)
}

fn native_api_output(method: &str, endpoint: &str, body: Option<&str>) -> Result<String> {
    let token = ["KNIT_GITLAB_TOKEN", "GITLAB_TOKEN"]
        .into_iter()
        .find_map(non_empty_env)
        .context("GitLab API access requires KNIT_GITLAB_TOKEN or GITLAB_TOKEN")?;
    let base = std::env::var("KNIT_GITLAB_API_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://gitlab.com/api/v4".to_string());
    let endpoint = endpoint.trim_start_matches('/');
    let operation = format!("{method} /{endpoint}");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(20))
        .resolver(ipv4_first_resolver as fn(&str) -> std::io::Result<Vec<std::net::SocketAddr>>)
        .build();
    let mut request = agent
        .request(method, &format!("{base}/{endpoint}"))
        .set("Accept", "application/json")
        .set("User-Agent", "knit")
        .set("PRIVATE-TOKEN", &token);
    if body.is_some() {
        request = request.set("Content-Type", "application/json");
    }
    let response = match body {
        Some(input) => request.send_string(input),
        None => request.call(),
    };
    match response {
        Ok(response) => response
            .into_string()
            .with_context(|| format!("failed to read GitLab API response for {operation}")),
        Err(ureq::Error::Status(status, response)) => {
            let detail = response.into_string().unwrap_or_default();
            if status == 401 || status == 403 {
                bail!(
                    "GitLab API request failed during {operation}: HTTP {status}: {}\nHint: set KNIT_GITLAB_TOKEN or GITLAB_TOKEN to a token with API access.",
                    detail.trim()
                );
            }
            bail!(
                "GitLab API request failed during {operation}: HTTP {status}: {}",
                detail.trim()
            );
        }
        Err(ureq::Error::Transport(error)) => {
            bail!("GitLab API request failed during {operation}: {error}")
        }
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn ipv4_first_resolver(netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    let all = netloc.to_socket_addrs()?.collect::<Vec<_>>();
    let v4 = all
        .iter()
        .copied()
        .filter(std::net::SocketAddr::is_ipv4)
        .collect::<Vec<_>>();
    Ok(if v4.is_empty() { all } else { v4 })
}

fn encode_query_component(input: &str) -> String {
    encode(input)
}

fn encode_path_component(input: &str) -> String {
    encode(input)
}

fn encode(input: &str) -> String {
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

fn pipeline_check(pipeline: Option<GlabPipeline>) -> Vec<CheckRun> {
    let Some(status) = pipeline.and_then(|pipeline| pipeline.status) else {
        return Vec::new();
    };
    let (state, bucket) = match status.as_str() {
        "success" => ("SUCCESS", "pass"),
        "failed" => ("FAILURE", "fail"),
        "canceled" | "cancelled" => ("CANCELLED", "cancel"),
        "skipped" | "manual" => ("SKIPPED", "skipping"),
        _ => ("RUNNING", "pending"),
    };
    vec![CheckRun {
        name: format!("pipeline ({status})"),
        state: Some(state.to_string()),
        bucket: Some(bucket.to_string()),
    }]
}

fn into_pull_request(mr: GlabMr) -> PullRequest {
    let draft = mr.draft.or(mr.work_in_progress).unwrap_or(false);
    let merge_status = mr.detailed_merge_status.or(mr.merge_status);
    PullRequest {
        number: mr.iid,
        url: mr.web_url,
        state: Some(normalize_state(mr.state.as_deref())),
        title: mr.title,
        base_ref_name: mr.target_branch,
        head_ref_name: mr.source_branch,
        body: mr.description,
        is_draft: Some(draft),
        head_ref_oid: mr.sha,
        mergeable: gitlab_mergeable(merge_status.as_deref()),
        merge_state_status: merge_status.map(|status| status.to_ascii_uppercase()),
        review_decision: None,
    }
}

fn gitlab_mergeable(status: Option<&str>) -> Option<String> {
    match status?.to_ascii_lowercase().as_str() {
        "mergeable" | "can_be_merged" => Some("MERGEABLE".to_string()),
        "conflict" | "conflicts" | "cannot_be_merged" => Some("CONFLICTING".to_string()),
        _ => None,
    }
}

/// Map GitLab MR state onto Knit's canonical uppercase states.
fn normalize_state(state: Option<&str>) -> String {
    match state.unwrap_or("").to_ascii_lowercase().as_str() {
        "opened" => "OPEN",
        "merged" => "MERGED",
        "closed" => "CLOSED",
        "locked" => "LOCKED",
        _ => "UNKNOWN",
    }
    .to_string()
}

/// `glab` accepts an MR IID; recover it from a recorded URL when needed.
fn selector_iid(selector: &str) -> String {
    if selector.chars().all(|ch| ch.is_ascii_digit()) && !selector.is_empty() {
        return selector.to_string();
    }
    selector
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| selector.to_string())
}

/// Parse `group/project` (including nested subgroups) from a GitLab remote URL.
pub(crate) fn full_name(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches(".git");
    let host = super::remote_host(remote)?;
    let index = remote.find(&host)?;
    let suffix = remote[index + host.len()..].trim_start_matches([':', '/']);
    if suffix.is_empty() || !suffix.contains('/') {
        return None;
    }
    Some(suffix.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_full_name() {
        assert_eq!(
            full_name("https://gitlab.com/acme/team/backend.git").as_deref(),
            Some("acme/team/backend")
        );
        assert_eq!(
            full_name("git@gitlab.com:acme/backend.git").as_deref(),
            Some("acme/backend")
        );
    }

    #[test]
    fn maps_mr_json_to_pull_request() {
        let json = r#"{"iid":12,"web_url":"https://gitlab.com/acme/backend/-/merge_requests/12","state":"opened","title":"t","target_branch":"main","source_branch":"knit/x","description":"body","draft":true,"sha":"deadbeef","head_pipeline":{"status":"running"}}"#;
        let mr: GlabMr = serde_json::from_str(json).unwrap();
        let pr = into_pull_request(mr);
        assert_eq!(pr.number, 12);
        assert_eq!(pr.state.as_deref(), Some("OPEN"));
        assert_eq!(pr.base_ref_name.as_deref(), Some("main"));
        assert_eq!(pr.is_draft, Some(true));
        assert_eq!(pr.head_ref_oid.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn pipeline_status_maps_to_check_bucket() {
        let runs = pipeline_check(Some(GlabPipeline {
            id: None,
            status: Some("failed".to_string()),
        }));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].bucket.as_deref(), Some("fail"));
        assert!(pipeline_check(None).is_empty());
    }

    #[test]
    fn selector_iid_recovers_from_url() {
        assert_eq!(
            selector_iid("https://gitlab.com/acme/backend/-/merge_requests/12"),
            "12"
        );
        assert_eq!(selector_iid("7"), "7");
    }
}
