//! GitHub forge adapter. This module implements the `Forge` trait on top of
//! the `gh` CLI; [`api`] holds the REST operations used when a repo full name
//! is known, and [`transport`] owns the HTTP/auth plumbing for the native
//! (CLI-free) API path.

mod api;
mod transport;

use super::{
    cli_output, parse_pr_url, repo_scoped_args, CheckRun, Forge, PrTarget, PullRequest,
    PULL_REQUEST_KIND,
};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use transport::use_native_github_api;

pub(super) const CLI: &str = "gh";
const PR_JSON_FIELDS: &str =
    "number,url,state,title,baseRefName,headRefName,body,isDraft,headRefOid,mergeable,mergeStateStatus,reviewDecision";

/// GitHub forge adapter, backed by the `gh` CLI.
pub struct GitHub;

/// CI verdict sources recorded for one commit SHA: GitHub check runs plus
/// commit statuses. Commit-scoped — unlike `Forge::check_runs`, no pull
/// request is involved.
pub fn commit_check_runs(
    target: &PrTarget,
    repo_full_name: &str,
    sha: &str,
) -> Result<Vec<CheckRun>> {
    api::commit_check_runs(target, repo_full_name, sha)
}

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
            return api::find_existing(target, repo_full_name, head, base);
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
            return api::create(target, repo_full_name, base, head, title, body, draft);
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
            return api::view(target, repo_full_name, selector);
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
            return api::edit_body(target, repo_full_name, selector, body);
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
                return api::merge(
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
                return api::check_runs(target, repo_full_name, selector, required_only);
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
            "gh pr checks https://github.com/acme/frontend/pull/45 --json name,state,bucket --required --repo acme/frontend failed in /tmp: GraphQL: Resource not accessible by personal access token (node.statusCheckRollup.nodes.0.commit.statusCheckRollup)"
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
            crate::providers::pr_number_from_url("https://github.com/acme/backend/pull/42"),
            Some(42)
        );
    }
}
