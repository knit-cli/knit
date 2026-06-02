use super::{
    cli_output, parse_pr_url, repo_scoped_args, CheckRun, Forge, PrTarget, PullRequest,
    MERGE_REQUEST_KIND,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::ffi::OsString;

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
    head_pipeline: Option<GlabPipeline>,
    #[serde(default)]
    pipeline: Option<GlabPipeline>,
}

#[derive(Debug, Deserialize)]
struct GlabPipeline {
    #[serde(default)]
    status: Option<String>,
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
        Ok(mrs.into_iter().next().map(into_pull_request))
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
        Ok(into_pull_request(mr))
    }

    fn edit_body(&self, target: &PrTarget, selector: &str, body: &str) -> Result<()> {
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

    fn merge(
        &self,
        target: &PrTarget,
        selector: &str,
        method: &str,
        delete_branch: bool,
        _match_head: Option<&str>,
    ) -> Result<()> {
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

    fn check_runs(
        &self,
        target: &PrTarget,
        selector: &str,
        _required_only: bool,
    ) -> Result<Vec<CheckRun>> {
        // GitLab exposes a single pipeline status per MR rather than discrete
        // required checks; surface it as one synthetic check.
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
        Ok(pipeline_check(mr.head_pipeline.or(mr.pipeline)))
    }
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
        mergeable: None,
        merge_state_status: None,
        review_decision: None,
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
