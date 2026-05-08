use crate::checkout::checkout_dir;
use crate::git::{current_branch, git_output, git_output_optional, rev_parse};
use crate::ids::short_sha;
use crate::model::{ChangeGroup, PublicationEntry, RepoEntry};
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, save_active_bundle, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const KNIT_PR_BLOCK_BEGIN: &str = "<!-- BEGIN KNIT BUNDLE -->";
const KNIT_PR_BLOCK_END: &str = "<!-- END KNIT BUNDLE -->";

const GITHUB_PROVIDER: &str = "github";
const GITHUB_PULL_REQUEST_KIND: &str = "pull_request";

pub fn create_github_publications(
    selectors: &[String],
    all: bool,
    draft: bool,
    sync: bool,
    set_upstream: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The active bundle has no repos. Run `knit track <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let mut failures = Vec::new();

    for index in indexes.iter().copied() {
        let repo = active.bundle.repos[index].clone();
        match create_or_reuse_pr(&mut active, &repo, draft, set_upstream) {
            Ok(()) => save_active_bundle(&active)?,
            Err(error) => {
                println!(
                    "{}: {}",
                    out::repo(&repo.id),
                    out::danger("PR create failed")
                );
                failures.push(format!("{}: {error:#}", repo.id));
                save_active_bundle(&active)?;
            }
        }
    }

    if sync {
        failures.extend(sync_github_publications_for_indexes(&mut active, &indexes)?);
    } else {
        println!(
            "{}",
            out::warn(
                "Skipped PR body sync. Run `knit publish github sync` to add cross-links later."
            )
        );
    }

    if !failures.is_empty() {
        bail!(
            "PR publishing completed with failures:\n{}",
            failures.join("\n")
        );
    }

    Ok(())
}

pub fn sync_github_publications(selectors: &[String], all: bool) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The active bundle has no repos. Run `knit track <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let failures = sync_github_publications_for_indexes(&mut active, &indexes)?;
    if !failures.is_empty() {
        bail!("PR sync completed with failures:\n{}", failures.join("\n"));
    }

    Ok(())
}

pub fn show_github_publication_status(selectors: &[String], all: bool) -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The active bundle has no repos. Run `knit track <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    println!("Bundle: {}\n", out::heading(&active.bundle.id));
    println!(
        "{}  {}  {}  {}",
        out::header_field("repo", 14),
        out::header_field("pr", 10),
        out::header_field("state", 12),
        out::heading("url")
    );

    for index in indexes {
        let repo = &active.bundle.repos[index];
        if let Some(pr) = publication_for_repo(&active.bundle, &repo.id) {
            println!(
                "{}  {}  {}  {}",
                out::repo_field(&repo.id, 14),
                out::sha(format!("#{}", pr.number)),
                out::status(&format!("{:<12}", pr.state.to_lowercase())),
                pr.url
            );
        } else {
            println!(
                "{}  {}  {}  {}",
                out::repo_field(&repo.id, 14),
                out::muted(format!("{:<10}", "(none)")),
                out::muted(format!("{:<12}", "not created")),
                out::muted("-")
            );
        }
    }

    Ok(())
}

fn create_or_reuse_pr(
    active: &mut ActiveBundle,
    repo: &RepoEntry,
    draft: bool,
    set_upstream: bool,
) -> Result<()> {
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded. Run `knit worktree`.",
            repo.id
        )
    })?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    ensure_feature_branch(repo, branch, &cwd)?;
    ensure_origin(repo, &cwd)?;

    let sha = rev_parse(&cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature branch HEAD", repo.id))?;
    run_push(&cwd, branch, set_upstream)
        .with_context(|| format!("{}: failed to push {branch}", repo.id))?;
    println!(
        "{}: {} {} {}",
        out::repo(&repo.id),
        out::movement("pushed"),
        out::branch(format!("origin/{branch}")),
        out::sha(short_sha(&sha))
    );

    if let Some(existing) = publication_for_repo(&active.bundle, &repo.id) {
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("exists"),
            existing.url
        );
        return Ok(());
    }

    if let Some(existing) = gh_find_existing_pr(&cwd, branch, &repo.base_branch)? {
        upsert_publication(active, repo, existing);
        let pr = publication_for_repo(&active.bundle, &repo.id).expect("PR was just inserted");
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("exists"),
            pr.url
        );
        return Ok(());
    }

    let title = format!("{} ({})", active.bundle.title, repo.id);
    let initial_body = initial_pr_body(&active.bundle, &repo.id);
    let url = gh_create_pr(
        &cwd,
        &repo.base_branch,
        branch,
        &title,
        &initial_body,
        draft,
    )?;
    let summary = gh_view_pr(&cwd, &url).unwrap_or_else(|_| GhPrSummary {
        number: pr_number_from_url(&url).unwrap_or(0),
        url: url.clone(),
        state: Some("OPEN".to_string()),
        title: Some(title),
        base_ref_name: Some(repo.base_branch.clone()),
        head_ref_name: Some(branch.to_string()),
        body: None,
    });
    upsert_publication(active, repo, summary);

    let pr = publication_for_repo(&active.bundle, &repo.id).expect("PR was just inserted");
    println!(
        "{}: {} #{} {}",
        out::repo(&repo.id),
        out::movement("created"),
        pr.number,
        pr.url
    );
    Ok(())
}

fn sync_github_publications_for_indexes(
    active: &mut ActiveBundle,
    indexes: &[usize],
) -> Result<Vec<String>> {
    let mut failures = Vec::new();

    for index in indexes.iter().copied() {
        let repo = active.bundle.repos[index].clone();
        match sync_one_pr(active, &repo) {
            Ok(()) => save_active_bundle(active)?,
            Err(error) => {
                println!("{}: {}", out::repo(&repo.id), out::danger("PR sync failed"));
                failures.push(format!("{}: {error:#}", repo.id));
                save_active_bundle(active)?;
            }
        }
    }

    Ok(failures)
}

fn sync_one_pr(active: &mut ActiveBundle, repo: &RepoEntry) -> Result<()> {
    let branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: no feature branch recorded. Run `knit worktree`.",
            repo.id
        )
    })?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };

    let summary = if let Some(pr) = publication_for_repo(&active.bundle, &repo.id) {
        gh_view_pr(&cwd, &pr.url)?
    } else if let Some(existing) = gh_find_existing_pr(&cwd, branch, &repo.base_branch)? {
        existing
    } else {
        println!("{}: {}", out::repo(&repo.id), out::muted("no PR recorded"));
        return Ok(());
    };

    upsert_publication(active, repo, summary.clone());
    let current_body = summary.body.unwrap_or_default();
    let block = render_knit_pr_block(&active.bundle, Some(&repo.id));
    let next_body = upsert_knit_pr_block(&current_body, &block);
    if next_body != current_body {
        let pr = publication_for_repo(&active.bundle, &repo.id).expect("PR was just inserted");
        gh_edit_pr_body(&cwd, &pr.url, &next_body)?;
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("synced"),
            pr.url
        );
    } else {
        println!(
            "{}: {}",
            out::repo(&repo.id),
            out::muted("PR body already synced")
        );
    }

    Ok(())
}

fn ensure_feature_branch(repo: &RepoEntry, expected: &str, cwd: &Path) -> Result<()> {
    let actual = current_branch(cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != expected {
        bail!(
            "{}: PR publishing expected feature branch `{expected}`, found `{actual}` in {}.",
            repo.id,
            cwd.display()
        );
    }

    Ok(())
}

fn ensure_origin(repo: &RepoEntry, cwd: &Path) -> Result<()> {
    git_output_optional(cwd, ["remote", "get-url", "origin"])?.with_context(|| {
        format!(
            "{}: no `origin` remote configured in {}",
            repo.id,
            cwd.display()
        )
    })?;
    Ok(())
}

fn run_push(cwd: &Path, branch: &str, set_upstream: bool) -> Result<()> {
    let mut args = vec![OsString::from("push")];
    if set_upstream {
        args.push(OsString::from("--set-upstream"));
    }
    args.push(OsString::from("origin"));
    args.push(OsString::from(branch));

    git_output(cwd, args)?;
    Ok(())
}

fn upsert_publication(active: &mut ActiveBundle, repo: &RepoEntry, summary: GhPrSummary) {
    let entry = PublicationEntry {
        repo_id: repo.id.clone(),
        provider: GITHUB_PROVIDER.to_string(),
        kind: GITHUB_PULL_REQUEST_KIND.to_string(),
        number: summary.number,
        url: summary.url,
        base_branch: summary
            .base_ref_name
            .unwrap_or_else(|| repo.base_branch.clone()),
        head_branch: summary
            .head_ref_name
            .or_else(|| repo.feature_branch.clone())
            .unwrap_or_default(),
        state: summary.state.unwrap_or_else(|| "UNKNOWN".to_string()),
        title: summary.title,
        updated_at: now_iso(),
    };

    if let Some(existing) = active.bundle.publications.iter_mut().find(|publication| {
        publication.provider == GITHUB_PROVIDER
            && publication.kind == GITHUB_PULL_REQUEST_KIND
            && publication.repo_id == repo.id
    }) {
        *existing = entry;
    } else {
        active.bundle.publications.push(entry);
    }
    active.bundle.updated_at = now_iso();
}

fn publication_for_repo<'a>(
    bundle: &'a ChangeGroup,
    repo_id: &str,
) -> Option<&'a PublicationEntry> {
    bundle.publications.iter().find(|publication| {
        publication.provider == GITHUB_PROVIDER
            && publication.kind == GITHUB_PULL_REQUEST_KIND
            && publication.repo_id == repo_id
    })
}

fn initial_pr_body(bundle: &ChangeGroup, current_repo_id: &str) -> String {
    format!(
        "Generated by Knit. Cross-links will be refreshed after all PRs are created.\n\n{}",
        render_knit_pr_block(bundle, Some(current_repo_id))
    )
}

fn render_knit_pr_block(bundle: &ChangeGroup, current_repo_id: Option<&str>) -> String {
    let mut lines = vec![
        KNIT_PR_BLOCK_BEGIN.to_string(),
        "## Knit Bundle".to_string(),
        String::new(),
        format!("This PR is part of Knit bundle `{}`.", bundle.id),
        String::new(),
        "See the other PRs in this bundle:".to_string(),
    ];

    for repo in &bundle.repos {
        match publication_for_repo(bundle, &repo.id) {
            Some(pr) => {
                let marker = if current_repo_id == Some(repo.id.as_str()) {
                    " (this PR)"
                } else {
                    ""
                };
                lines.push(format!("- `{}`: {}{}", repo.id, pr.url, marker));
            }
            None => lines.push(format!("- `{}`: pending", repo.id)),
        }
    }

    lines.extend([
        String::new(),
        format!("Bundle id: `{}`", bundle.id),
        format!("Bundle title: {}", bundle.title),
        KNIT_PR_BLOCK_END.to_string(),
    ]);

    lines.join("\n")
}

fn upsert_knit_pr_block(existing_body: &str, block: &str) -> String {
    let Some(begin) = existing_body.find(KNIT_PR_BLOCK_BEGIN) else {
        return append_knit_pr_block(existing_body, block);
    };
    let Some(relative_end) = existing_body[begin..].find(KNIT_PR_BLOCK_END) else {
        return append_knit_pr_block(existing_body, block);
    };
    let end = begin + relative_end + KNIT_PR_BLOCK_END.len();

    let before = existing_body[..begin].trim_end();
    let after = existing_body[end..].trim_start();
    match (before.is_empty(), after.is_empty()) {
        (true, true) => block.to_string(),
        (true, false) => format!("{block}\n\n{after}"),
        (false, true) => format!("{before}\n\n{block}"),
        (false, false) => format!("{before}\n\n{block}\n\n{after}"),
    }
}

fn append_knit_pr_block(existing_body: &str, block: &str) -> String {
    if existing_body.trim().is_empty() {
        block.to_string()
    } else {
        format!("{}\n\n{}", existing_body.trim_end(), block)
    }
}

fn gh_find_existing_pr(cwd: &Path, branch: &str, base_branch: &str) -> Result<Option<GhPrSummary>> {
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
            OsString::from("number,url,state,title,baseRefName,headRefName,body"),
            OsString::from("--limit"),
            OsString::from("1"),
        ],
        None,
    )?;
    let prs: Vec<GhPrSummary> =
        serde_json::from_str(&output).context("failed to parse `gh pr list` JSON")?;
    Ok(prs.into_iter().next())
}

fn gh_create_pr(
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

fn gh_view_pr(cwd: &Path, url: &str) -> Result<GhPrSummary> {
    let output = gh_output(
        cwd,
        [
            OsString::from("pr"),
            OsString::from("view"),
            OsString::from(url),
            OsString::from("--json"),
            OsString::from("number,url,state,title,baseRefName,headRefName,body"),
        ],
        None,
    )?;
    serde_json::from_str(&output).context("failed to parse `gh pr view` JSON")
}

fn gh_edit_pr_body(cwd: &Path, url: &str, body: &str) -> Result<()> {
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
                "failed to run `gh {}` in {}. Install and authenticate GitHub CLI to use `knit publish github`.",
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
            .context("failed to write PR body to GitHub CLI")?;
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

fn pr_number_from_url(url: &str) -> Option<u64> {
    url.rsplit('/').next()?.parse().ok()
}

fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPrSummary {
    number: u64,
    url: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    base_ref_name: Option<String>,
    #[serde(default)]
    head_ref_name: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CHANGE_GROUP_KIND, SCHEMA_VERSION};

    #[test]
    fn managed_block_is_replaced_without_touching_user_body() {
        let previous = format!("Intro\n\n{KNIT_PR_BLOCK_BEGIN}\nold\n{KNIT_PR_BLOCK_END}\n\nTail");
        let next = upsert_knit_pr_block(&previous, "new block");
        assert_eq!(next, "Intro\n\nnew block\n\nTail");
    }

    #[test]
    fn rendered_block_lists_known_and_pending_prs() {
        let mut bundle = ChangeGroup {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: CHANGE_GROUP_KIND.to_string(),
            id: "venue-capacity".to_string(),
            title: "venue capacity".to_string(),
            created_at: "2026-05-05T00:00:00.000Z".to_string(),
            updated_at: "2026-05-05T00:00:00.000Z".to_string(),
            head_node_id: None,
            repos: vec![
                RepoEntry {
                    id: "backend".to_string(),
                    path: "/tmp/backend".to_string(),
                    remote: None,
                    base_branch: "main".to_string(),
                    checkout_mode: "worktree".to_string(),
                    base_sha: None,
                    feature_branch: Some("knit/venue-capacity".to_string()),
                    worktree_path: None,
                    head_sha: None,
                },
                RepoEntry {
                    id: "frontend".to_string(),
                    path: "/tmp/frontend".to_string(),
                    remote: None,
                    base_branch: "main".to_string(),
                    checkout_mode: "worktree".to_string(),
                    base_sha: None,
                    feature_branch: Some("knit/venue-capacity".to_string()),
                    worktree_path: None,
                    head_sha: None,
                },
            ],
            commit_groups: Vec::new(),
            nodes: Vec::new(),
            publications: vec![PublicationEntry {
                repo_id: "backend".to_string(),
                provider: GITHUB_PROVIDER.to_string(),
                kind: GITHUB_PULL_REQUEST_KIND.to_string(),
                number: 123,
                url: "https://github.com/acme/backend/pull/123".to_string(),
                base_branch: "main".to_string(),
                head_branch: "knit/venue-capacity".to_string(),
                state: "OPEN".to_string(),
                title: None,
                updated_at: "2026-05-05T00:00:00.000Z".to_string(),
            }],
        };

        let block = render_knit_pr_block(&bundle, Some("backend"));
        assert!(block.contains("This PR is part of Knit bundle `venue-capacity`."));
        assert!(block.contains("`backend`: https://github.com/acme/backend/pull/123 (this PR)"));
        assert!(block.contains("`frontend`: pending"));

        bundle.publications.push(PublicationEntry {
            repo_id: "frontend".to_string(),
            provider: GITHUB_PROVIDER.to_string(),
            kind: GITHUB_PULL_REQUEST_KIND.to_string(),
            number: 456,
            url: "https://github.com/acme/frontend/pull/456".to_string(),
            base_branch: "main".to_string(),
            head_branch: "knit/venue-capacity".to_string(),
            state: "OPEN".to_string(),
            title: None,
            updated_at: "2026-05-05T00:00:00.000Z".to_string(),
        });
        let synced = render_knit_pr_block(&bundle, Some("backend"));
        assert!(synced.contains("`frontend`: https://github.com/acme/frontend/pull/456"));
    }
}
