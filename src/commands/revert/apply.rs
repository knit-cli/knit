//! Execute a revert plan: preflight every repo, then apply local
//! revert/cherry-pick operations or provider PR reverts, recording the
//! resulting revert group in the bundle ledger.

use super::*;
use crate::checkout::checkout_dir;
use crate::git::{commit_author, git_output, rev_parse};
use crate::ids::{revert_group_id, short_sha};
use crate::model::{BundleNode, CommitAuthor, CommitGroup, CommitRef, Movement, RepoChange};
use crate::output as out;
use crate::providers::{self, pr_number_from_url, PrTarget, PullRequest};
use crate::store::{save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

pub(super) fn apply_local_revert(
    active: &mut ActiveBundle,
    plan: &RevertPlan,
    path: PathBuf,
) -> Result<()> {
    let group_id = revert_group_id();
    let created_at = now_iso();
    let logical_message = format!("Revert {}", plan.target_message);
    let commit_message = format!(
        "{logical_message}\n\nKnit-Reverts: {}\nKnit-Group: {group_id}\nKnit-Bundle: {}",
        plan.target_node_id, active.bundle.id
    );
    let mut commits = Vec::new();
    let mut repo_changes = Vec::new();
    let mut group_author: Option<CommitAuthor> = None;

    for repo_plan in &plan.repos {
        let (repo_index, worktree) = repo_context(active, &repo_plan.repo_id)?;
        let before_sha = rev_parse(&worktree, "HEAD")
            .with_context(|| format!("{}: failed to read current HEAD", repo_plan.repo_id))?;

        for operation in &repo_plan.operations {
            apply_operation(&worktree, &repo_plan.repo_id, operation)?;
        }

        git_output(&worktree, ["add", "-A"])
            .with_context(|| format!("{}: failed to stage revert changes", repo_plan.repo_id))?;
        let status = git_output(&worktree, ["status", "--short"])?;
        if status.trim().is_empty() {
            println!(
                "{}: {}",
                out::repo(&repo_plan.repo_id),
                out::muted("revert produced no file changes")
            );
            continue;
        }

        git_output(
            &worktree,
            [
                OsString::from("commit"),
                OsString::from("-m"),
                OsString::from(&commit_message),
            ],
        )
        .with_context(|| format!("{}: failed to commit revert", repo_plan.repo_id))?;

        let sha = rev_parse(&worktree, "HEAD")
            .with_context(|| format!("{}: failed to read revert commit sha", repo_plan.repo_id))?;
        if group_author.is_none() {
            group_author = Some(commit_author(&worktree, &sha).with_context(|| {
                format!("{}: failed to read revert commit author", repo_plan.repo_id)
            })?);
        }
        println!(
            "{}: {} {}",
            out::repo(&repo_plan.repo_id),
            out::movement("committed"),
            out::sha(short_sha(&sha))
        );
        commits.push(CommitRef {
            repo_id: repo_plan.repo_id.clone(),
            sha: sha.clone(),
        });
        repo_changes.push(RepoChange {
            repo_id: repo_plan.repo_id.clone(),
            movement: Movement::Advanced,
            before_sha: Some(before_sha),
            after_sha: sha.clone(),
            commits: vec![sha.clone()],
            dropped_commits: Vec::new(),
        });
        active.bundle.repos[repo_index].head_sha = Some(sha);
    }

    if commits.is_empty() {
        bail!("Revert produced no commits.");
    }

    active.bundle.commit_groups.push(CommitGroup {
        id: group_id.clone(),
        message: logical_message.clone(),
        created_at: created_at.clone(),
        commits: commits.clone(),
        author: group_author,
    });
    active.bundle.nodes.push(BundleNode::revert_group(
        group_id.clone(),
        created_at,
        plan.target_node_id.clone(),
        logical_message,
        commits,
        repo_changes,
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(active)?;
    let _ = fs::remove_file(path);

    println!(
        "{} {}",
        out::heading("Recorded revert group"),
        out::node(group_id)
    );
    Ok(())
}

pub(super) fn apply_provider_revert(
    active: &mut ActiveBundle,
    plan: &RevertPlan,
    path: PathBuf,
) -> Result<()> {
    let mut items = Vec::new();
    for repo_plan in &plan.repos {
        for operation in &repo_plan.operations {
            let selector = operation_selector(operation)?;
            items.push((repo_plan.repo_id.clone(), selector.to_string()));
        }
    }

    create_provider_revert_prs(
        active,
        plan.provider.as_deref(),
        &plan.target_node_id,
        &plan.target_message,
        &items,
    )?;
    let _ = fs::remove_file(path);
    Ok(())
}

/// Create provider-side revert PRs for a group of merged PRs and record the
/// `pr.revert` ledger node. `items` pairs each repo id with the merged PR's
/// selector (URL). Shared by `knit revert --apply` (landed nodes) and
/// `knit land rollback` (half-failed land runs). Returns the revert group id.
pub(crate) fn create_provider_revert_prs(
    active: &mut ActiveBundle,
    provider: Option<&str>,
    target_node_id: &str,
    target_message: &str,
    items: &[(String, String)],
) -> Result<String> {
    let group_id = revert_group_id();
    let created_at = now_iso();
    let logical_message = format!("Revert {target_message}");
    let mut repo_ids = BTreeSet::new();
    let mut publication_urls = BTreeSet::new();
    let mut failures = Vec::new();

    for (repo_id, selector) in items {
        let (repo_index, target, forge) =
            provider_revert_context(active, provider, repo_id, Some(selector))?;
        let repo = active.bundle.repos[repo_index].clone();

        let title = format!("Revert {} ({})", active.bundle.title, repo_id);
        let body = format!(
            "Reverts {selector}\n\nKnit-Reverts: {target_node_id}\nKnit-Group: {group_id}\nKnit-Bundle: {}",
            active.bundle.id
        );
        match forge.revert_pull_request(&target, selector, &title, &body) {
            Ok(url) => {
                let summary = forge.view(&target, &url).unwrap_or_else(|_| PullRequest {
                    number: pr_number_from_url(&url).unwrap_or(0),
                    url: url.clone(),
                    state: Some("OPEN".to_string()),
                    title: Some(title.clone()),
                    base_ref_name: Some(repo.base_branch.clone()),
                    head_ref_name: None,
                    body: None,
                    is_draft: None,
                    head_ref_oid: None,
                    mergeable: None,
                    merge_state_status: None,
                    review_decision: None,
                });
                providers::upsert_publication(&mut active.bundle, &repo, forge.as_ref(), &summary);
                repo_ids.insert(repo_id.clone());
                publication_urls.insert(summary.url.clone());
                println!(
                    "{}: {} {}",
                    out::repo(repo_id),
                    out::movement("revert PR"),
                    summary.url
                );
            }
            Err(error) => failures.push(format!("{repo_id}: {error:#}")),
        }
    }

    if !failures.is_empty() {
        save_active_bundle(active)?;
        bail!(
            "PR revert completed with failures:\n{}",
            failures.join("\n")
        );
    }
    if publication_urls.is_empty() {
        bail!("PR revert produced no review objects.");
    }

    let provider = provider.unwrap_or("provider").to_string();
    active.bundle.nodes.push(BundleNode::pr_revert(
        group_id.clone(),
        created_at,
        target_node_id.to_string(),
        logical_message,
        provider,
        repo_ids.into_iter().collect(),
        publication_urls.into_iter().collect(),
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(active)?;

    println!(
        "{} {}",
        out::heading("Recorded PR revert group"),
        out::node(&group_id)
    );
    Ok(group_id)
}

pub(super) fn preflight_plan(active: &ActiveBundle, plan: &RevertPlan) -> Result<()> {
    for repo_plan in &plan.repos {
        if repo_plan
            .operations
            .iter()
            .any(|operation| operation.kind == RevertOpKind::PrRevert)
        {
            preflight_provider_revert(active, plan, repo_plan)?;
            continue;
        }

        let (_, worktree) = repo_context(active, &repo_plan.repo_id)?;
        let current_head = rev_parse(&worktree, "HEAD")
            .with_context(|| format!("{}: failed to read current HEAD", repo_plan.repo_id))?;
        let expected_head_sha = repo_plan.expected_head_sha.as_ref().with_context(|| {
            format!(
                "{}: revert plan is missing expectedHeadSha for local git operations",
                repo_plan.repo_id
            )
        })?;
        if current_head != expected_head_sha.as_str() {
            bail!(
                "{}: HEAD changed since the revert plan was written (expected {}, found {}). Re-run `knit revert {}`.",
                repo_plan.repo_id,
                short_sha(expected_head_sha),
                short_sha(&current_head),
                plan.target_ref
            );
        }

        let status = git_output(&worktree, ["status", "--short"])?;
        if !status.trim().is_empty() {
            bail!(
                "{}: worktree must be clean before applying a Knit revert.",
                repo_plan.repo_id
            );
        }

        for operation in &repo_plan.operations {
            verify_operation(&worktree, &repo_plan.repo_id, operation)?;
        }
    }

    Ok(())
}

fn verify_operation(worktree: &PathBuf, repo_id: &str, operation: &RevertOperation) -> Result<()> {
    if !matches!(
        operation.kind,
        RevertOpKind::Revert | RevertOpKind::CherryPick
    ) {
        bail!("{repo_id}: unknown revert operation `{}`.", operation.kind);
    }
    let sha = operation_sha(operation)?;

    git_output(
        worktree,
        [
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from(format!("{sha}^{{commit}}")),
        ],
    )
    .with_context(|| {
        format!(
            "{repo_id}: commit {} is not available locally",
            short_sha(sha)
        )
    })?;
    Ok(())
}

fn preflight_provider_revert(
    active: &ActiveBundle,
    plan: &RevertPlan,
    repo_plan: &RepoRevertPlan,
) -> Result<()> {
    if repo_plan
        .operations
        .iter()
        .any(|operation| operation.kind != RevertOpKind::PrRevert)
    {
        bail!(
            "{}: provider PR revert operations cannot be mixed with local git operations.",
            repo_plan.repo_id
        );
    }

    let selector_hint = repo_plan
        .operations
        .iter()
        .find_map(|operation| operation.selector.as_deref());
    let (_, target, forge) = provider_revert_context(
        active,
        plan.provider.as_deref(),
        &repo_plan.repo_id,
        selector_hint,
    )?;
    for operation in &repo_plan.operations {
        let selector = operation_selector(operation)?;
        let pr = forge
            .view(&target, selector)
            .with_context(|| format!("{}: failed to load {}", repo_plan.repo_id, selector))?;
        if !pull_request_is_merged(&pr) {
            bail!(
                "{}: PR #{} is {}, expected MERGED before provider revert.",
                repo_plan.repo_id,
                pr.number,
                pr.state.as_deref().unwrap_or("UNKNOWN")
            );
        }
    }

    Ok(())
}

fn apply_operation(worktree: &PathBuf, repo_id: &str, operation: &RevertOperation) -> Result<()> {
    let sha = operation_sha(operation)?;
    match operation.kind {
        RevertOpKind::Revert => git_output(
            worktree,
            [
                OsString::from("revert"),
                OsString::from("--no-commit"),
                OsString::from(sha),
            ],
        )
        .with_context(|| {
            format!(
                "{repo_id}: failed to revert {}. Resolve the git state manually before retrying.",
                short_sha(sha)
            )
        })?,
        RevertOpKind::CherryPick => git_output(
            worktree,
            [
                OsString::from("cherry-pick"),
                OsString::from("--no-commit"),
                OsString::from(sha),
            ],
        )
        .with_context(|| {
            format!(
                "{repo_id}: failed to cherry-pick {}. Resolve the git state manually before retrying.",
                short_sha(sha)
            )
        })?,
        RevertOpKind::PrRevert => {
            bail!("{repo_id}: provider PR reverts are not applied as local git operations.")
        }
    };

    Ok(())
}

fn operation_sha(operation: &RevertOperation) -> Result<&str> {
    operation
        .sha
        .as_deref()
        .with_context(|| format!("{} operation is missing sha", operation.kind))
}

fn operation_selector(operation: &RevertOperation) -> Result<&str> {
    if operation.kind != RevertOpKind::PrRevert {
        bail!("{} operation is not a provider PR revert", operation.kind);
    }
    operation
        .selector
        .as_deref()
        .with_context(|| "provider PR revert operation is missing selector")
}

pub(super) fn plan_uses_provider_revert(plan: &RevertPlan) -> bool {
    plan.repos.iter().any(|repo| {
        repo.operations
            .iter()
            .any(|operation| operation.kind == RevertOpKind::PrRevert)
    })
}

/// Resolve the forge and PR target for provider-side operations on a repo,
/// preferring an explicit provider id over per-repo remote detection.
pub(crate) fn provider_revert_context(
    active: &ActiveBundle,
    provider: Option<&str>,
    repo_id: &str,
    selector_hint: Option<&str>,
) -> Result<(usize, PrTarget, Box<dyn providers::Forge>)> {
    let (index, repo) = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .find(|(_, repo)| repo.id == repo_id)
        .with_context(|| format!("{repo_id}: repo is no longer tracked in this bundle"))?;
    let forge = match provider {
        Some(provider) => providers::by_id(provider)
            .with_context(|| format!("{repo_id}: unknown provider `{provider}`"))?,
        None => providers::for_repo(repo)?,
    };
    let target = provider_target(active, repo, forge.as_ref(), selector_hint)?;

    Ok((index, target, forge))
}

fn provider_target(
    active: &ActiveBundle,
    repo: &crate::model::RepoEntry,
    forge: &dyn providers::Forge,
    selector_hint: Option<&str>,
) -> Result<PrTarget> {
    if let Some(full_name) = repo
        .remote
        .as_deref()
        .and_then(|remote| forge.repo_full_name(remote))
    {
        return Ok(PrTarget::explicit(active.root.clone(), full_name));
    }
    if let Some(cwd) = checkout_dir(active, repo) {
        return Ok(PrTarget::checkout(cwd));
    }
    if let Some(full_name) = selector_hint.and_then(repo_full_name_from_pr_url) {
        return Ok(PrTarget::explicit(active.root.clone(), full_name));
    }
    bail!(
        "{}: no checkout or parseable remote is available for provider PR revert.",
        repo.id
    );
}

fn repo_full_name_from_pr_url(selector: &str) -> Option<String> {
    let path = selector
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| path))?;
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let marker = segments
        .iter()
        .position(|segment| matches!(*segment, "pull" | "pulls" | "merge_requests"))?;
    let mut repo_segments = &segments[..marker];
    if repo_segments.last() == Some(&"-") {
        repo_segments = &repo_segments[..repo_segments.len().saturating_sub(1)];
    }
    (repo_segments.len() >= 2).then(|| repo_segments.join("/"))
}

fn pull_request_is_merged(pr: &PullRequest) -> bool {
    pr.state.as_deref() == Some("MERGED")
}
