use super::{
    cli_output, parse_pr_url, repo_scoped_args, CheckRun, Forge, PrTarget, PullRequest,
    PULL_REQUEST_KIND,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::ffi::OsString;

const CLI: &str = "tea";
const LIST_FIELDS: &str = "index,state,title,head,base,url";

/// Codeberg / Forgejo / Gitea adapter, backed by the `tea` CLI.
///
/// `tea` exposes pull requests; its `--output json` keys vary across versions, so
/// the JSON model below accepts several aliases. Commit-status checks are not
/// surfaced by `tea`, so landing treats Forgejo PRs as having no required checks.
pub struct Forgejo;

#[derive(Debug, Default, Deserialize)]
struct TeaPr {
    #[serde(default, alias = "Index", alias = "number", alias = "Number")]
    index: Option<u64>,
    #[serde(default, alias = "URL", alias = "html_url", alias = "HTMLURL")]
    url: Option<String>,
    #[serde(default, alias = "State")]
    state: Option<String>,
    #[serde(default, alias = "Title")]
    title: Option<String>,
    #[serde(default, alias = "Head")]
    head: Option<String>,
    #[serde(default, alias = "Base")]
    base: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForgejoApiPr {
    #[serde(default, alias = "index")]
    number: u64,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    head: ForgejoApiRef,
    base: ForgejoApiRef,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    mergeable: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ForgejoApiRef {
    #[serde(rename = "ref")]
    ref_name: String,
    #[serde(default)]
    sha: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForgejoReview {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForgejoStatusCollection {
    #[serde(default)]
    statuses: Option<Vec<ForgejoStatus>>,
}

#[derive(Debug, Deserialize)]
struct ForgejoStatus {
    #[serde(default)]
    context: Option<String>,
    state: String,
}

impl Forge for Forgejo {
    fn id(&self) -> &'static str {
        "forgejo"
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
        if use_api(target) {
            let repo = resolve_repo(target)?;
            let output = api_output(
                target,
                "GET",
                &format!("repos/{repo}/pulls?state=all&limit=50"),
                None,
            )?;
            let prs: Vec<ForgejoApiPr> =
                serde_json::from_str(&output).context("failed to parse Forgejo pull list JSON")?;
            return prs
                .into_iter()
                .find(|pr| pr.head.ref_name == head && pr.base.ref_name == base)
                .map(|pr| enrich_api_pr(target, &repo, pr))
                .transpose();
        }
        let found = self
            .list(target, "all")?
            .into_iter()
            .find(|pr| pr.head.as_deref() == Some(head) && pr.base.as_deref() == Some(base));
        Ok(found.map(into_pull_request))
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
        let title = if draft && !title.starts_with("Draft:") {
            format!("Draft: {title}")
        } else {
            title.to_string()
        };
        if use_api(target) {
            let repo = resolve_repo(target)?;
            let payload = serde_json::to_string(&json!({
                "head": head,
                "base": base,
                "title": title,
                "body": body,
            }))
            .context("failed to encode Forgejo pull request payload")?;
            let output = api_output(
                target,
                "POST",
                &format!("repos/{repo}/pulls"),
                Some(&payload),
            )?;
            let pr: ForgejoApiPr = serde_json::from_str(&output)
                .context("failed to parse Forgejo pull create JSON")?;
            return Ok(pr.html_url);
        }
        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("pr"),
                OsString::from("create"),
                OsString::from("--head"),
                OsString::from(head),
                OsString::from("--base"),
                OsString::from(base),
                OsString::from("--title"),
                OsString::from(&title),
                OsString::from("--description"),
                OsString::from(body),
            ],
        );
        let output = cli_output(CLI, &target.cwd, args, None)?;
        if let Some(url) = parse_pr_url(&output) {
            return Ok(url);
        }
        // Some `tea` versions print only a confirmation; recover the URL by listing.
        self.find_existing(target, head, base)?
            .map(|pr| pr.url)
            .context("`tea pr create` did not print a PR URL")
    }

    fn view(&self, target: &PrTarget, selector: &str) -> Result<PullRequest> {
        if use_api(target) {
            let repo = resolve_repo(target)?;
            let output = api_output(
                target,
                "GET",
                &format!("repos/{repo}/pulls/{}", selector_index(selector)),
                None,
            )?;
            let pr: ForgejoApiPr =
                serde_json::from_str(&output).context("failed to parse Forgejo pull JSON")?;
            return enrich_api_pr(target, &repo, pr);
        }
        let index = selector_index(selector);
        self.list(target, "all")?
            .into_iter()
            .find(|pr| pr.index.map(|value| value.to_string()).as_deref() == Some(&index))
            .map(into_pull_request)
            .with_context(|| format!("no Forgejo PR found for selector `{selector}`"))
    }

    fn edit_body(&self, target: &PrTarget, selector: &str, body: &str) -> Result<()> {
        if use_api(target) {
            return edit_api_pr(target, selector, &json!({ "body": body }));
        }
        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("pr"),
                OsString::from("edit"),
                OsString::from(selector_index(selector)),
                OsString::from("--description"),
                OsString::from(body),
            ],
        );
        cli_output(CLI, &target.cwd, args, None)?;
        Ok(())
    }

    fn edit_base(&self, target: &PrTarget, selector: &str, base: &str) -> Result<()> {
        edit_api_pr(target, selector, &json!({ "base": base }))
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
                        "Forgejo PR {selector} head `{actual}` does not match expected `{expected}`; refusing to merge."
                    );
                }
            }
        }
        let style = match method {
            "merge" => "merge",
            "squash" => "squash",
            "rebase" => "rebase",
            other => bail!("unknown Forgejo merge method `{other}`"),
        };
        if use_api(target) {
            let repo = resolve_repo(target)?;
            let payload = serde_json::to_string(&json!({
                "Do": style,
                "delete_branch_after_merge": delete_branch,
            }))
            .context("failed to encode Forgejo merge payload")?;
            api_output(
                target,
                "POST",
                &format!("repos/{repo}/pulls/{}/merge", selector_index(selector)),
                Some(&payload),
            )?;
            return Ok(());
        }
        let mut args = vec![
            OsString::from("pr"),
            OsString::from("merge"),
            OsString::from(selector_index(selector)),
            OsString::from("--style"),
            OsString::from(style),
        ];
        if delete_branch {
            args.push(OsString::from("--delete-branch"));
        }
        let args = repo_scoped_args(target, "--repo", args);
        cli_output(CLI, &target.cwd, args, None)?;
        Ok(())
    }

    fn check_runs(
        &self,
        target: &PrTarget,
        selector: &str,
        _required_only: bool,
    ) -> Result<Vec<CheckRun>> {
        if !use_api(target) {
            // Basic tea-only users can still publish and land; richer status
            // evidence requires an API token.
            return Ok(Vec::new());
        }
        let repo = resolve_repo(target)?;
        let sha = self
            .view(target, selector)?
            .head_ref_oid
            .filter(|sha| !sha.is_empty())
            .with_context(|| format!("could not determine Forgejo PR head for `{selector}`"))?;
        commit_check_runs(target, &repo, &sha)
    }
}

impl Forgejo {
    fn list(&self, target: &PrTarget, state: &str) -> Result<Vec<TeaPr>> {
        let args = repo_scoped_args(
            target,
            "--repo",
            vec![
                OsString::from("pr"),
                OsString::from("list"),
                OsString::from("--state"),
                OsString::from(state),
                OsString::from("--fields"),
                OsString::from(LIST_FIELDS),
                OsString::from("--output"),
                OsString::from("json"),
            ],
        );
        let output = cli_output(CLI, &target.cwd, args, None)?;
        if output.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&output).context("failed to parse `tea pr list` JSON")
    }
}

fn use_api(target: &PrTarget) -> bool {
    target.repo_full_name.is_some() || api_token().is_some()
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
                "could not resolve Forgejo repository: no origin remote in {}",
                target.cwd.display()
            )
        })?;
    full_name(&remote).with_context(|| {
        format!(
            "could not parse a Forgejo owner/repository from origin `{}`",
            remote.trim()
        )
    })
}

fn enrich_api_pr(target: &PrTarget, repo: &str, pr: ForgejoApiPr) -> Result<PullRequest> {
    let index = pr.number;
    let approved = api_output(
        target,
        "GET",
        &format!("repos/{repo}/pulls/{index}/reviews"),
        None,
    )
    .ok()
    .and_then(|output| serde_json::from_str::<Vec<ForgejoReview>>(&output).ok())
    .is_some_and(|reviews| {
        reviews.iter().any(|review| {
            review
                .state
                .as_deref()
                .is_some_and(|state| state.eq_ignore_ascii_case("APPROVED"))
        })
    });
    Ok(PullRequest {
        number: pr.number,
        url: pr.html_url,
        state: Some(if pr.merged {
            "MERGED".to_string()
        } else {
            normalize_state(pr.state.as_deref())
        }),
        title: pr.title,
        base_ref_name: Some(pr.base.ref_name),
        head_ref_name: Some(pr.head.ref_name),
        body: pr.body,
        is_draft: pr.draft,
        head_ref_oid: pr.head.sha,
        mergeable: pr.mergeable.map(|mergeable| {
            if mergeable {
                "MERGEABLE".to_string()
            } else {
                "CONFLICTING".to_string()
            }
        }),
        merge_state_status: None,
        review_decision: approved.then(|| "APPROVED".to_string()),
    })
}

fn edit_api_pr(target: &PrTarget, selector: &str, value: &serde_json::Value) -> Result<()> {
    let repo = resolve_repo(target)?;
    let payload =
        serde_json::to_string(value).context("failed to encode Forgejo pull edit payload")?;
    api_output(
        target,
        "PATCH",
        &format!("repos/{repo}/pulls/{}", selector_index(selector)),
        Some(&payload),
    )?;
    Ok(())
}

pub(crate) fn commit_check_runs(target: &PrTarget, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
    let output = api_output(
        target,
        "GET",
        &format!("repos/{repo}/commits/{}/status", encode_path_component(sha)),
        None,
    )?;
    let collection: ForgejoStatusCollection =
        serde_json::from_str(&output).context("failed to parse Forgejo commit status JSON")?;
    Ok(collection
        .statuses
        .unwrap_or_default()
        .into_iter()
        .map(Into::into)
        .collect())
}

impl From<ForgejoStatus> for CheckRun {
    fn from(status: ForgejoStatus) -> Self {
        let (state, bucket) = forgejo_status_bucket(&status.state);
        CheckRun {
            name: status.context.unwrap_or_else(|| "status".to_string()),
            state: Some(state.to_string()),
            bucket: Some(bucket.to_string()),
        }
    }
}

fn forgejo_status_bucket(status: &str) -> (&'static str, &'static str) {
    match status.to_ascii_lowercase().as_str() {
        "success" => ("SUCCESS", "pass"),
        "failure" | "error" => ("FAILURE", "fail"),
        "cancelled" | "canceled" => ("CANCELLED", "cancel"),
        "warning" => ("SKIPPED", "skipping"),
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
    let token = api_token().context(
        "Forgejo API access requires KNIT_FORGEJO_TOKEN, CODEBERG_TOKEN, or GITEA_TOKEN",
    )?;
    let base = api_base(target)?;
    let endpoint = endpoint.trim_start_matches('/');
    let operation = format!("{method} /{endpoint}");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .resolver(ipv4_first_resolver as fn(&str) -> std::io::Result<Vec<std::net::SocketAddr>>)
        .build();
    for attempt in 0..3 {
        let mut request = agent
            .request(method, &format!("{base}/{endpoint}"))
            .set("Accept", "application/json")
            .set("User-Agent", "knit")
            .set("Authorization", &format!("token {token}"));
        if body.is_some() {
            request = request.set("Content-Type", "application/json");
        }
        let response = match body {
            Some(input) => request.send_string(input),
            None => request.call(),
        };
        match response {
            Ok(response) => {
                return response.into_string().with_context(|| {
                    format!("failed to read Forgejo API response for {operation}")
                });
            }
            Err(ureq::Error::Status(status, response)) => {
                let detail = response.into_string().unwrap_or_default();
                if (500..=599).contains(&status) && attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
                    continue;
                }
                if status == 401 || status == 403 {
                    bail!(
                        "Forgejo API request failed during {operation}: HTTP {status}: {}\nHint: set KNIT_FORGEJO_TOKEN, CODEBERG_TOKEN, or GITEA_TOKEN to a repository-capable token.",
                        detail.trim()
                    );
                }
                bail!(
                    "Forgejo API request failed during {operation}: HTTP {status}: {}",
                    detail.trim()
                );
            }
            Err(ureq::Error::Transport(error)) => {
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
                    continue;
                }
                bail!("Forgejo API request failed during {operation}: {error}")
            }
        }
    }
    unreachable!("Forgejo API retry loop always returns or errors")
}

fn api_base(target: &PrTarget) -> Result<String> {
    if let Some(base) = std::env::var("KNIT_FORGEJO_API_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok(base);
    }
    if target.repo_full_name.is_none() {
        if let Some(remote) =
            crate::git::git_output_optional(&target.cwd, ["remote", "get-url", "origin"])?
        {
            if let Some(host) = super::remote_host(&remote) {
                return Ok(format!("https://{host}/api/v1"));
            }
        }
    }
    Ok("https://codeberg.org/api/v1".to_string())
}

fn api_token() -> Option<String> {
    ["KNIT_FORGEJO_TOKEN", "CODEBERG_TOKEN", "GITEA_TOKEN"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
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

fn encode_path_component(input: &str) -> String {
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

fn into_pull_request(pr: TeaPr) -> PullRequest {
    PullRequest {
        number: pr.index.unwrap_or(0),
        url: pr.url.unwrap_or_default(),
        state: Some(normalize_state(pr.state.as_deref())),
        title: pr.title,
        base_ref_name: pr.base,
        head_ref_name: pr.head,
        body: None,
        is_draft: Some(false),
        head_ref_oid: None,
        mergeable: None,
        merge_state_status: None,
        review_decision: None,
    }
}

/// Map Forgejo/Gitea PR state onto Knit's canonical uppercase states.
fn normalize_state(state: Option<&str>) -> String {
    match state.unwrap_or("").to_ascii_lowercase().as_str() {
        "open" => "OPEN",
        "merged" => "MERGED",
        "closed" => "CLOSED",
        _ => "UNKNOWN",
    }
    .to_string()
}

fn selector_index(selector: &str) -> String {
    if !selector.is_empty() && selector.chars().all(|ch| ch.is_ascii_digit()) {
        return selector.to_string();
    }
    selector
        .trim_start_matches('#')
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(|segment| segment.trim_start_matches('#').to_string())
        .unwrap_or_else(|| selector.to_string())
}

/// Parse `owner/name` from a Codeberg/Forgejo remote URL.
pub(crate) fn full_name(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches(".git");
    let host = super::remote_host(remote)?;
    let index = remote.find(&host)?;
    let suffix = remote[index + host.len()..].trim_start_matches([':', '/']);
    let (owner, rest) = suffix.split_once('/')?;
    let name = rest.split('/').next().unwrap_or(rest);
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_name() {
        assert_eq!(
            full_name("https://codeberg.org/acme/backend.git").as_deref(),
            Some("acme/backend")
        );
        assert_eq!(
            full_name("git@codeberg.org:acme/backend.git").as_deref(),
            Some("acme/backend")
        );
    }

    #[test]
    fn maps_tea_json_with_aliased_keys() {
        let json = r#"[{"Index":4,"State":"open","Title":"t","Head":"knit/x","Base":"main","URL":"https://codeberg.org/acme/backend/pulls/4"}]"#;
        let prs: Vec<TeaPr> = serde_json::from_str(json).unwrap();
        let pr = into_pull_request(prs.into_iter().next().unwrap());
        assert_eq!(pr.number, 4);
        assert_eq!(pr.state.as_deref(), Some("OPEN"));
        assert_eq!(pr.head_ref_name.as_deref(), Some("knit/x"));
    }

    #[test]
    fn selector_index_recovers_from_url() {
        assert_eq!(
            selector_index("https://codeberg.org/acme/backend/pulls/4"),
            "4"
        );
        assert_eq!(selector_index("#9"), "9");
        assert_eq!(selector_index("5"), "5");
    }

    #[test]
    fn null_commit_statuses_are_treated_as_empty() {
        let collection: ForgejoStatusCollection =
            serde_json::from_str(r#"{"statuses":null}"#).unwrap();
        assert!(collection.statuses.unwrap_or_default().is_empty());
    }
}
