use super::{pr_number_from_url, CheckRun, Forge, PrTarget, PullRequest, PULL_REQUEST_KIND};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

const API_BASE: &str = "https://api.bitbucket.org/2.0";

/// Bitbucket Cloud adapter. Bitbucket has no official general-purpose CLI, so
/// Knit talks to the Cloud 2.0 REST API directly.
pub struct Bitbucket;

#[derive(Debug, Deserialize)]
struct BitbucketPullRequest {
    id: u64,
    links: BitbucketLinks,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    state: Option<String>,
    source: BitbucketRef,
    destination: BitbucketRef,
    #[serde(default)]
    summary: Option<BitbucketText>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    participants: Vec<BitbucketParticipant>,
}

#[derive(Debug, Deserialize)]
struct BitbucketLinks {
    html: BitbucketHref,
}

#[derive(Debug, Deserialize)]
struct BitbucketHref {
    href: String,
}

#[derive(Debug, Deserialize)]
struct BitbucketRef {
    branch: BitbucketBranch,
    #[serde(default)]
    commit: Option<BitbucketCommit>,
}

#[derive(Debug, Deserialize)]
struct BitbucketBranch {
    name: String,
}

#[derive(Debug, Deserialize)]
struct BitbucketCommit {
    #[serde(default)]
    hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BitbucketText {
    #[serde(default)]
    raw: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BitbucketParticipant {
    #[serde(default)]
    approved: bool,
}

#[derive(Debug, Deserialize)]
struct BitbucketList<T> {
    values: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct BitbucketStatus {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    name: Option<String>,
    state: String,
}

impl Forge for Bitbucket {
    fn id(&self) -> &'static str {
        "bitbucket"
    }

    fn review_kind(&self) -> &'static str {
        PULL_REQUEST_KIND
    }

    fn cli(&self) -> &'static str {
        "bitbucket"
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
        let repo = resolve_repo(target)?;
        let query = pull_request_query(head, base);
        let endpoint = format!(
            "repositories/{}/pullrequests?q={}&pagelen=1&sort=-updated_on",
            encode_repo(&repo)?,
            encode_query_component(&query)
        );
        let output = api_output("GET", &endpoint, None)?;
        let list: BitbucketList<BitbucketPullRequest> =
            serde_json::from_str(&output).context("failed to parse Bitbucket pull list JSON")?;
        Ok(list
            .values
            .into_iter()
            .next()
            .map(BitbucketPullRequest::into_pull_request))
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
        let repo = resolve_repo(target)?;
        let payload = serde_json::to_string(&json!({
            "title": title,
            "description": body,
            "source": { "branch": { "name": head } },
            "destination": { "branch": { "name": base } },
            "draft": draft,
        }))
        .context("failed to encode Bitbucket pull request payload")?;
        let output = api_output(
            "POST",
            &format!("repositories/{}/pullrequests", encode_repo(&repo)?),
            Some(&payload),
        )?;
        let pr: BitbucketPullRequest =
            serde_json::from_str(&output).context("failed to parse Bitbucket pull create JSON")?;
        Ok(pr.links.html.href)
    }

    fn view(&self, target: &PrTarget, selector: &str) -> Result<PullRequest> {
        let repo = resolve_repo(target)?;
        let id = selector_id(selector)
            .with_context(|| format!("could not determine Bitbucket PR id from `{selector}`"))?;
        let output = api_output(
            "GET",
            &format!("repositories/{}/pullrequests/{id}", encode_repo(&repo)?),
            None,
        )?;
        let pr: BitbucketPullRequest =
            serde_json::from_str(&output).context("failed to parse Bitbucket pull JSON")?;
        Ok(pr.into_pull_request())
    }

    fn edit_body(&self, target: &PrTarget, selector: &str, body: &str) -> Result<()> {
        self.edit(
            target,
            selector,
            &serde_json::to_string(&json!({ "description": body }))
                .context("failed to encode Bitbucket pull request edit payload")?,
        )
    }

    fn edit_base(&self, target: &PrTarget, selector: &str, base: &str) -> Result<()> {
        self.edit(
            target,
            selector,
            &serde_json::to_string(&json!({ "destination": { "branch": { "name": base } } }))
                .context("failed to encode Bitbucket pull request target payload")?,
        )
    }

    fn merge(
        &self,
        target: &PrTarget,
        selector: &str,
        method: &str,
        delete_branch: bool,
        match_head: Option<&str>,
    ) -> Result<()> {
        let repo = resolve_repo(target)?;
        let id = selector_id(selector)
            .with_context(|| format!("could not determine Bitbucket PR id from `{selector}`"))?;
        if let Some(expected) = match_head.filter(|sha| !sha.is_empty()) {
            let actual = self.view(target, selector)?.head_ref_oid;
            if let Some(actual) = actual.filter(|sha| !sha.is_empty()) {
                if !sha_matches(expected, &actual) {
                    bail!(
                        "Bitbucket PR {selector} head `{actual}` does not match expected `{expected}`; refusing to merge."
                    );
                }
            }
        }
        let payload = serde_json::to_string(&json!({
            "merge_strategy": merge_strategy(method)?,
            "close_source_branch": delete_branch,
        }))
        .context("failed to encode Bitbucket merge payload")?;
        api_output(
            "POST",
            &format!(
                "repositories/{}/pullrequests/{id}/merge",
                encode_repo(&repo)?
            ),
            Some(&payload),
        )?;
        Ok(())
    }

    fn check_runs(
        &self,
        target: &PrTarget,
        selector: &str,
        _required_only: bool,
    ) -> Result<Vec<CheckRun>> {
        let repo = resolve_repo(target)?;
        let id = selector_id(selector)
            .with_context(|| format!("could not determine Bitbucket PR id from `{selector}`"))?;
        let output = api_output(
            "GET",
            &format!(
                "repositories/{}/pullrequests/{id}/statuses?pagelen=100",
                encode_repo(&repo)?
            ),
            None,
        )?;
        let list: BitbucketList<BitbucketStatus> =
            serde_json::from_str(&output).context("failed to parse Bitbucket statuses JSON")?;
        Ok(list.values.into_iter().map(Into::into).collect())
    }
}

impl Bitbucket {
    fn edit(&self, target: &PrTarget, selector: &str, payload: &str) -> Result<()> {
        let repo = resolve_repo(target)?;
        let id = selector_id(selector)
            .with_context(|| format!("could not determine Bitbucket PR id from `{selector}`"))?;
        api_output(
            "PUT",
            &format!("repositories/{}/pullrequests/{id}", encode_repo(&repo)?),
            Some(payload),
        )?;
        Ok(())
    }
}

impl BitbucketPullRequest {
    fn into_pull_request(self) -> PullRequest {
        let approved = self
            .participants
            .iter()
            .any(|participant| participant.approved);
        PullRequest {
            number: self.id,
            url: self.links.html.href,
            state: Some(normalize_state(self.state.as_deref())),
            title: self.title,
            base_ref_name: Some(self.destination.branch.name),
            head_ref_name: Some(self.source.branch.name),
            body: self
                .summary
                .and_then(|summary| summary.raw)
                .or(self.description),
            is_draft: self.draft,
            head_ref_oid: self.source.commit.and_then(|commit| commit.hash),
            mergeable: None,
            merge_state_status: None,
            review_decision: approved.then(|| "APPROVED".to_string()),
        }
    }
}

impl From<BitbucketStatus> for CheckRun {
    fn from(status: BitbucketStatus) -> Self {
        let (state, bucket) = status_bucket(&status.state);
        CheckRun {
            name: status
                .name
                .or(status.key)
                .unwrap_or_else(|| "status".to_string()),
            state: Some(state.to_string()),
            bucket: Some(bucket.to_string()),
        }
    }
}

pub(crate) fn commit_check_runs(target: &PrTarget, repo: &str, sha: &str) -> Result<Vec<CheckRun>> {
    let _ = target;
    let output = api_output(
        "GET",
        &format!(
            "repositories/{}/commit/{}/statuses/build?pagelen=100",
            encode_repo(repo)?,
            encode_path_component(sha)
        ),
        None,
    )?;
    let list: BitbucketList<BitbucketStatus> =
        serde_json::from_str(&output).context("failed to parse Bitbucket commit statuses JSON")?;
    Ok(list.values.into_iter().map(Into::into).collect())
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
                "could not resolve Bitbucket repository: no origin remote in {}",
                target.cwd.display()
            )
        })?;
    full_name(&remote).with_context(|| {
        format!(
            "could not parse a Bitbucket workspace/repository from origin `{}`",
            remote.trim()
        )
    })
}

pub(crate) fn full_name(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches(".git");
    let host = super::remote_host(remote)?;
    let index = remote.find(&host)?;
    let suffix = remote[index + host.len()..].trim_start_matches([':', '/']);
    let mut parts = suffix.split('/').filter(|part| !part.is_empty());
    let workspace = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(format!("{workspace}/{repo}"))
}

fn normalize_state(state: Option<&str>) -> String {
    match state.unwrap_or("").to_ascii_uppercase().as_str() {
        "OPEN" => "OPEN",
        "MERGED" => "MERGED",
        "DECLINED" | "SUPERSEDED" => "CLOSED",
        _ => "UNKNOWN",
    }
    .to_string()
}

fn status_bucket(status: &str) -> (&'static str, &'static str) {
    match status.to_ascii_uppercase().as_str() {
        "SUCCESSFUL" => ("SUCCESS", "pass"),
        "FAILED" => ("FAILURE", "fail"),
        "STOPPED" => ("CANCELLED", "cancel"),
        _ => ("RUNNING", "pending"),
    }
}

fn pull_request_query(head: &str, base: &str) -> String {
    let head = head.replace('"', "\\\"");
    let base = base.replace('"', "\\\"");
    format!(
        "source.branch.name = \"{head}\" AND destination.branch.name = \"{base}\" AND (state = \"OPEN\" OR state = \"MERGED\" OR state = \"DECLINED\" OR state = \"SUPERSEDED\")"
    )
}

fn selector_id(selector: &str) -> Option<u64> {
    selector
        .trim()
        .parse()
        .ok()
        .or_else(|| pr_number_from_url(selector.trim_end_matches('/')))
}

fn merge_strategy(method: &str) -> Result<&'static str> {
    match method {
        "merge" => Ok("merge_commit"),
        "squash" => Ok("squash"),
        // Bitbucket's nearest equivalent to rebase is fast-forward, which
        // correctly fails if the branch cannot be fast-forwarded.
        "rebase" => Ok("fast_forward"),
        other => bail!("unknown Bitbucket merge method `{other}`"),
    }
}

fn sha_matches(expected: &str, actual: &str) -> bool {
    expected.starts_with(actual) || actual.starts_with(expected)
}

fn encode_repo(repo: &str) -> Result<String> {
    let (workspace, slug) = repo
        .split_once('/')
        .filter(|(workspace, slug)| !workspace.is_empty() && !slug.is_empty())
        .with_context(|| format!("invalid Bitbucket repository name `{repo}`"))?;
    Ok(format!(
        "{}/{}",
        encode_path_component(workspace),
        encode_path_component(slug)
    ))
}

fn encode_path_component(input: &str) -> String {
    encode_query_component(input)
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

fn api_base() -> String {
    std::env::var("KNIT_BITBUCKET_API_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| API_BASE.to_string())
}

fn auth_header() -> Result<String> {
    if let Some(token) = non_empty_env("KNIT_BITBUCKET_ACCESS_TOKEN") {
        return Ok(format!("Bearer {token}"));
    }
    if let (Some(email), Some(token)) = (
        non_empty_env("KNIT_BITBUCKET_EMAIL"),
        non_empty_env("KNIT_BITBUCKET_API_TOKEN"),
    ) {
        return Ok(format!(
            "Basic {}",
            base64_encode(&format!("{email}:{token}"))
        ));
    }
    bail!(
        "Bitbucket authentication requires KNIT_BITBUCKET_ACCESS_TOKEN, or both KNIT_BITBUCKET_EMAIL and KNIT_BITBUCKET_API_TOKEN."
    )
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn api_output(method: &str, endpoint: &str, body: Option<&str>) -> Result<String> {
    let auth = auth_header()?;
    let endpoint = endpoint.trim_start_matches('/');
    let operation = format!("{method} /{endpoint}");
    let url = format!("{}/{endpoint}", api_base());
    // Keep this small transport local for now. It mirrors the proven GitHub
    // transport; extracting a shared authenticated client can happen separately.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(20))
        .resolver(ipv4_first_resolver as fn(&str) -> std::io::Result<Vec<std::net::SocketAddr>>)
        .build();
    let mut request = agent
        .request(method, &url)
        .set("Accept", "application/json")
        .set("User-Agent", "knit")
        .set("Authorization", &auth);
    if body.is_some() {
        request = request.set("Content-Type", "application/json");
    }
    let result = match body {
        Some(input) => request.send_string(input),
        None => request.call(),
    };
    match result {
        Ok(response) => response
            .into_string()
            .with_context(|| format!("failed to read Bitbucket API response for {operation}")),
        Err(ureq::Error::Status(status, response)) => {
            let detail = response.into_string().unwrap_or_default();
            if status == 401 || status == 403 {
                bail!(
                    "Bitbucket API request failed during {operation}: HTTP {status}: {}\nHint: set KNIT_BITBUCKET_ACCESS_TOKEN, or KNIT_BITBUCKET_EMAIL with KNIT_BITBUCKET_API_TOKEN, to credentials that can access this repository.",
                    detail.trim()
                );
            }
            bail!(
                "Bitbucket API request failed during {operation}: HTTP {status}: {}",
                detail.trim()
            );
        }
        Err(ureq::Error::Transport(error)) => {
            bail!("Bitbucket API request failed during {operation}: {error}")
        }
    }
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

fn base64_encode(input: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or_default() as u32;
        let third = chunk.get(2).copied().unwrap_or_default() as u32;
        let bits = (first << 16) | (second << 8) | third;
        output.push(TABLE[((bits >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((bits >> 12) & 0x3f) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((bits >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(bits & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_name() {
        assert_eq!(
            full_name("https://bitbucket.org/acme/backend.git").as_deref(),
            Some("acme/backend")
        );
        assert_eq!(
            full_name("git@bitbucket.org:acme/backend.git").as_deref(),
            Some("acme/backend")
        );
        assert_eq!(full_name("https://bitbucket.org/acme").as_deref(), None);
    }

    #[test]
    fn maps_state_and_status() {
        assert_eq!(normalize_state(Some("OPEN")), "OPEN");
        assert_eq!(normalize_state(Some("SUPERSEDED")), "CLOSED");
        assert_eq!(status_bucket("SUCCESSFUL"), ("SUCCESS", "pass"));
        assert_eq!(status_bucket("FAILED"), ("FAILURE", "fail"));
        assert_eq!(status_bucket("INPROGRESS"), ("RUNNING", "pending"));
    }

    #[test]
    fn builds_escaped_query() {
        assert_eq!(
            pull_request_query("knit/\"quoted\"", "main"),
            "source.branch.name = \"knit/\\\"quoted\\\"\" AND destination.branch.name = \"main\" AND (state = \"OPEN\" OR state = \"MERGED\" OR state = \"DECLINED\" OR state = \"SUPERSEDED\")"
        );
    }

    #[test]
    fn selector_recovers_id() {
        assert_eq!(selector_id("42"), Some(42));
        assert_eq!(
            selector_id("https://bitbucket.org/acme/backend/pull-requests/42"),
            Some(42)
        );
    }

    #[test]
    fn maps_merge_strategy() {
        assert_eq!(merge_strategy("merge").unwrap(), "merge_commit");
        assert_eq!(merge_strategy("squash").unwrap(), "squash");
        assert_eq!(merge_strategy("rebase").unwrap(), "fast_forward");
        assert!(merge_strategy("octopus").is_err());
    }

    #[test]
    fn basic_auth_encoding_is_standard_base64() {
        assert_eq!(
            base64_encode("user@example.test:token"),
            "dXNlckBleGFtcGxlLnRlc3Q6dG9rZW4="
        );
    }

    #[test]
    fn accepts_short_or_full_matching_sha() {
        assert!(sha_matches("deadbeefcafebabe", "deadbeefcafe"));
        assert!(!sha_matches("deadbeefcafebabe", "baadf00dcafe"));
    }
}
