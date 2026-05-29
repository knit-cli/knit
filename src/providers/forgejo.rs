use super::{
    cli_output, parse_pr_url, repo_scoped_args, CheckRun, Forge, PrTarget, PullRequest,
    PULL_REQUEST_KIND,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
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
        _draft: bool,
    ) -> Result<String> {
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
                OsString::from(title),
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
        let index = selector_index(selector);
        self.list(target, "all")?
            .into_iter()
            .find(|pr| pr.index.map(|value| value.to_string()).as_deref() == Some(&index))
            .map(into_pull_request)
            .with_context(|| format!("no Forgejo PR found for selector `{selector}`"))
    }

    fn edit_body(&self, target: &PrTarget, selector: &str, body: &str) -> Result<()> {
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

    fn merge(
        &self,
        target: &PrTarget,
        selector: &str,
        method: &str,
        delete_branch: bool,
        _match_head: Option<&str>,
    ) -> Result<()> {
        let style = match method {
            "merge" => "merge",
            "squash" => "squash",
            "rebase" => "rebase",
            other => bail!("unknown Forgejo merge method `{other}`"),
        };
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
        _target: &PrTarget,
        _selector: &str,
        _required_only: bool,
    ) -> Result<Vec<CheckRun>> {
        // `tea` does not expose commit-status checks; report none so landing
        // proceeds (Codeberg/Forgejo typically gate by review, not checks).
        Ok(Vec::new())
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
}
