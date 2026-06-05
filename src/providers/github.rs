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
    let output = cli_output(
        CLI,
        &target.cwd,
        vec![OsString::from("api"), OsString::from(endpoint)],
        None,
    )?;
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
    let output = cli_output(
        CLI,
        &target.cwd,
        vec![OsString::from("api"), OsString::from(endpoint)],
        None,
    )?;
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
    let args = vec![
        OsString::from("api"),
        OsString::from("--method"),
        OsString::from("PATCH"),
        OsString::from(pull_request_api_item_endpoint(repo_full_name, number)),
        OsString::from("--input"),
        OsString::from("-"),
    ];
    cli_output(CLI, &target.cwd, args, Some(&payload))?;
    Ok(())
}

fn pull_request_api_endpoint(repo_full_name: &str) -> String {
    format!("repos/{repo_full_name}/pulls")
}

fn pull_request_api_item_endpoint(repo_full_name: &str, number: u64) -> String {
    format!("{}/{number}", pull_request_api_endpoint(repo_full_name))
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
    let message = error.to_string().to_lowercase();
    message.contains("resource not accessible by personal access token")
        || message.contains("resource not accessible by integration")
        || message.contains("insufficient_scope")
        || (message.contains("graphql") && message.contains("not accessible"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
