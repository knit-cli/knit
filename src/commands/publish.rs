use crate::checkout::checkout_dir;
use crate::git::{current_branch, git_output, git_output_optional, rev_parse};
use crate::ids::short_sha;
use crate::model::{ChangeGroup, RepoEntry};
use crate::output as out;
use crate::providers::github::{
    self, create_pr, edit_pr_body, find_existing_pr, pr_number_from_url, publication_for_repo,
    view_pr, PullRequest,
};
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, save_active_bundle, ActiveBundle,
};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

const KNIT_PR_BLOCK_BEGIN: &str = "<!-- BEGIN KNIT BUNDLE -->";
const KNIT_PR_BLOCK_END: &str = "<!-- END KNIT BUNDLE -->";

pub fn create_github_publications(
    selectors: &[String],
    all: bool,
    draft: bool,
    bases: &[String],
    sync: bool,
    set_upstream: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let base_overrides = BaseOverrides::parse(bases)?;
    base_overrides.validate_tracked_repos(&active.bundle)?;
    let mut failures = Vec::new();

    for index in indexes.iter().copied() {
        let repo = active.bundle.repos[index].clone();
        let base_branch =
            base_overrides.branch_for(&repo, publication_for_repo(&active.bundle, &repo.id));
        match create_or_reuse_pr(&mut active, &repo, &base_branch, draft, set_upstream) {
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
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
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
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
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
    base_branch: &str,
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
        if existing.base_branch != base_branch {
            bail!(
                "{}: PR already recorded against {}. Knit records one PR per repo in a bundle; create a new bundle or publish before changing the base.",
                repo.id,
                out::branch(&existing.base_branch)
            );
        }
        println!(
            "{}: {} {}",
            out::repo(&repo.id),
            out::movement("exists"),
            existing.url
        );
        return Ok(());
    }

    if let Some(existing) = find_existing_pr(&cwd, branch, base_branch)? {
        github::upsert_publication(&mut active.bundle, repo, &existing);
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
    let url = create_pr(&cwd, base_branch, branch, &title, &initial_body, draft)?;
    let summary = view_pr(&cwd, &url).unwrap_or_else(|_| PullRequest {
        number: pr_number_from_url(&url).unwrap_or(0),
        url: url.clone(),
        state: Some("OPEN".to_string()),
        title: Some(title),
        base_ref_name: Some(base_branch.to_string()),
        head_ref_name: Some(branch.to_string()),
        body: None,
        is_draft: None,
        head_ref_oid: None,
    });
    github::upsert_publication(&mut active.bundle, repo, &summary);

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

#[derive(Debug, Default)]
struct BaseOverrides {
    default: Option<String>,
    per_repo: BTreeMap<String, String>,
}

impl BaseOverrides {
    fn parse(values: &[String]) -> Result<Self> {
        let mut overrides = Self::default();
        for value in values {
            let value = value.trim();
            if value.is_empty() {
                bail!("--base cannot be empty.");
            }
            if let Some((repo_id, branch)) = value.split_once('=') {
                let repo_id = repo_id.trim();
                let branch = branch.trim();
                if repo_id.is_empty() || branch.is_empty() {
                    bail!("Use --base REPO=BRANCH with both sides present.");
                }
                overrides
                    .per_repo
                    .insert(crate::ids::slugify(repo_id), branch.to_string());
            } else if overrides.default.replace(value.to_string()).is_some() {
                bail!("Pass only one default --base value, or use repeated --base REPO=BRANCH overrides.");
            }
        }
        Ok(overrides)
    }

    fn branch_for(
        &self,
        repo: &RepoEntry,
        existing: Option<&crate::model::PublicationEntry>,
    ) -> String {
        self.per_repo
            .get(&repo.id)
            .or(self.default.as_ref())
            .cloned()
            .or_else(|| existing.map(|publication| publication.base_branch.clone()))
            .unwrap_or_else(|| repo.base_branch.clone())
    }

    fn validate_tracked_repos(&self, bundle: &ChangeGroup) -> Result<()> {
        for repo_id in self.per_repo.keys() {
            if !bundle.repos.iter().any(|repo| &repo.id == repo_id) {
                bail!("--base references unknown repo `{repo_id}`.");
            }
        }
        Ok(())
    }
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
        view_pr(&cwd, &pr.url)?
    } else if let Some(existing) = find_existing_pr(&cwd, branch, &repo.base_branch)? {
        existing
    } else {
        println!("{}: {}", out::repo(&repo.id), out::muted("no PR recorded"));
        return Ok(());
    };

    github::upsert_publication(&mut active.bundle, repo, &summary);
    let current_body = summary.body.unwrap_or_default();
    let block = render_knit_pr_block(&active.bundle, Some(&repo.id));
    let next_body = upsert_knit_pr_block(&current_body, &block);
    if next_body != current_body {
        let pr = publication_for_repo(&active.bundle, &repo.id).expect("PR was just inserted");
        edit_pr_body(&cwd, &pr.url, &next_body)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PublicationEntry, CHANGE_GROUP_KIND, SCHEMA_VERSION};

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
            state: Some(crate::model::BUNDLE_STATE_OPEN.to_string()),
            closed_at: None,
            archived_at: None,
            deleted_at: None,
            project_id: None,
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
                provider: github::PROVIDER.to_string(),
                kind: github::PULL_REQUEST_KIND.to_string(),
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
            provider: github::PROVIDER.to_string(),
            kind: github::PULL_REQUEST_KIND.to_string(),
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
