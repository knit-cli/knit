use crate::checkout::{checkout_dir, ensure_expected_branch};
use crate::git::{commit_author, git_output, rev_parse};
use crate::ids::{revert_group_id, revert_plan_id, short_sha};
use crate::model::{
    BundleNode, CommitAuthor, CommitGroup, CommitRef, Movement, RepoChange, SCHEMA_VERSION,
};
use crate::output as out;
use crate::providers::{self, pr_number_from_url, publication_for_repo, PrTarget, PullRequest};
use crate::selectors::resolve_log_node;
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, read_json, save_active_bundle, write_json,
    ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

const REVERT_PLAN_KIND: &str = "KnitRevertPlan";
/// One operation in a revert plan. Serialized camelCase to match recorded
/// plans (`revert`, `cherryPick`, `prRevert`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum RevertOpKind {
    Revert,
    CherryPick,
    PrRevert,
}

impl RevertOpKind {
    fn as_str(self) -> &'static str {
        match self {
            RevertOpKind::Revert => "revert",
            RevertOpKind::CherryPick => "cherryPick",
            RevertOpKind::PrRevert => "prRevert",
        }
    }
}

impl std::fmt::Display for RevertOpKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevertPlan {
    schema_version: String,
    kind: String,
    id: String,
    bundle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    target_ref: String,
    target_node_id: String,
    target_node_type: String,
    target_message: String,
    created_at: String,
    repos: Vec<RepoRevertPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoRevertPlan {
    repo_id: String,
    worktree_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_head_sha: Option<String>,
    operations: Vec<RevertOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevertOperation {
    kind: RevertOpKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selector: Option<String>,
}

pub fn revert_target(target: &str, apply: bool) -> Result<()> {
    if apply {
        apply_revert(target)
    } else {
        plan_revert(target)
    }
}

fn plan_revert(target: &str) -> Result<()> {
    let active = load_active_bundle()?;
    let plan = build_plan(&active, target)?;
    preflight_plan(&active, &plan)?;

    let path = plan_path(&active, &plan.target_node_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_json(&path, &plan)?;
    print_plan(&plan, &path);
    Ok(())
}

fn apply_revert(target: &str) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let target_node = resolve_log_node(&active.bundle.nodes, target)?.clone();
    let path = plan_path(&active, &target_node.id);

    if !path.exists() {
        bail!(
            "No revert plan found for {}. Run `knit revert {target}` first, inspect the plan, then run `knit revert {target} --apply`.",
            target_node.id
        );
    }

    let plan: RevertPlan = read_json(&path)?;
    if plan.bundle_id != active.bundle.id {
        bail!(
            "Revert plan belongs to bundle {}, but resolved bundle is {}.",
            plan.bundle_id,
            active.bundle.id
        );
    }
    if plan.target_node_id != target_node.id {
        bail!(
            "Revert plan targets {}, but {target} resolves to {} now. Re-run the plan.",
            plan.target_node_id,
            target_node.id
        );
    }

    preflight_plan(&active, &plan)?;
    if plan_uses_provider_revert(&plan) {
        return apply_provider_revert(&mut active, &plan, path);
    }

    apply_local_revert(&mut active, &plan, path)
}

fn apply_local_revert(active: &mut ActiveBundle, plan: &RevertPlan, path: PathBuf) -> Result<()> {
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

fn apply_provider_revert(
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
        let (repo_index, target, forge) = provider_revert_context(active, provider, repo_id)?;
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

fn build_plan(active: &ActiveBundle, target: &str) -> Result<RevertPlan> {
    let target_node = resolve_log_node(&active.bundle.nodes, target)?;
    let mut provider = None;
    let mut repos = match target_node.node_type.as_str() {
        "commit.group" | "revert.group" => plans_for_commits(active, &target_node.commits)?,
        "git.observed" | "land.update" => plans_for_observed(active, target_node)?,
        "feature.landed" => {
            provider = target_node.provider.clone();
            plans_for_landed_prs(active, target_node)?
        }
        "repo.removed" => bail!(
            "{} is a metadata node. Knit cannot safely restore a removed repo entry because bundle nodes do not yet store the full removed repo record.",
            target_node.id
        ),
        node_type => bail!("Cannot revert node {} of type {node_type}.", target_node.id),
    };

    repos.retain(|repo| !repo.operations.is_empty());
    if repos.is_empty() {
        bail!("Node {} has no revertable repo operations.", target_node.id);
    }

    Ok(RevertPlan {
        schema_version: SCHEMA_VERSION.to_string(),
        kind: REVERT_PLAN_KIND.to_string(),
        id: revert_plan_id(),
        bundle_id: active.bundle.id.clone(),
        provider,
        target_ref: target.to_string(),
        target_node_id: target_node.id.clone(),
        target_node_type: target_node.node_type.clone(),
        target_message: node_message(target_node),
        created_at: now_iso(),
        repos,
    })
}

fn plans_for_commits(active: &ActiveBundle, commits: &[CommitRef]) -> Result<Vec<RepoRevertPlan>> {
    let mut by_repo: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for commit in commits {
        by_repo
            .entry(commit.repo_id.clone())
            .or_default()
            .push(commit.sha.clone());
    }

    by_repo
        .into_iter()
        .map(|(repo_id, shas)| {
            let operations = shas
                .into_iter()
                .rev()
                .map(|sha| RevertOperation {
                    kind: RevertOpKind::Revert,
                    sha: Some(sha),
                    selector: None,
                })
                .collect();
            repo_plan(active, &repo_id, operations)
        })
        .collect()
}

fn plans_for_observed(active: &ActiveBundle, node: &BundleNode) -> Result<Vec<RepoRevertPlan>> {
    node.repo_changes
        .iter()
        .map(|change| {
            let mut operations = Vec::new();
            match change.movement.as_str() {
                "advanced" => {
                    operations.extend(change.commits.iter().rev().map(|sha| RevertOperation {
                        kind: RevertOpKind::Revert,
                        sha: Some(sha.clone()),
                        selector: None,
                    }));
                }
                "rewound" => {
                    operations.extend(change.dropped_commits.iter().map(|sha| RevertOperation {
                        kind: RevertOpKind::CherryPick,
                        sha: Some(sha.clone()),
                        selector: None,
                    }));
                }
                "diverged" => {
                    operations.extend(change.commits.iter().rev().map(|sha| RevertOperation {
                        kind: RevertOpKind::Revert,
                        sha: Some(sha.clone()),
                        selector: None,
                    }));
                    operations.extend(change.dropped_commits.iter().map(|sha| RevertOperation {
                        kind: RevertOpKind::CherryPick,
                        sha: Some(sha.clone()),
                        selector: None,
                    }));
                }
                movement => bail!(
                    "Cannot revert observed movement `{movement}` for repo {}.",
                    change.repo_id
                ),
            }
            repo_plan(active, &change.repo_id, operations)
        })
        .collect()
}

fn plans_for_landed_prs(active: &ActiveBundle, node: &BundleNode) -> Result<Vec<RepoRevertPlan>> {
    let repo_ids = node
        .repo_ids
        .as_ref()
        .filter(|ids| !ids.is_empty())
        .with_context(|| format!("landed node {} does not record repo ids", node.id))?;
    let landed_urls = node
        .publication_urls
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();

    repo_ids
        .iter()
        .map(|repo_id| {
            let publication = publication_for_repo(&active.bundle, repo_id).with_context(|| {
                format!(
                    "{repo_id}: no current review publication recorded for landed node {}",
                    node.id
                )
            })?;
            if !landed_urls.is_empty() && !landed_urls.contains(publication.url.as_str()) {
                bail!(
                    "{}: current review publication {} is not one of the PRs recorded on landed node {}. Knit can only provider-revert the current landed PR group.",
                    repo_id,
                    publication.url,
                    node.id
                );
            }
            if let Some(provider) = &node.provider {
                if publication.provider != *provider {
                    bail!(
                        "{}: landed node provider is {}, but publication {} uses {}.",
                        repo_id,
                        provider,
                        publication.url,
                        publication.provider
                    );
                }
            }
            pr_revert_repo_plan(active, repo_id, &publication.url)
        })
        .collect()
}

fn repo_plan(
    active: &ActiveBundle,
    repo_id: &str,
    operations: Vec<RevertOperation>,
) -> Result<RepoRevertPlan> {
    let (_, worktree) = repo_context(active, repo_id)?;
    let expected_head_sha = rev_parse(&worktree, "HEAD")
        .with_context(|| format!("{repo_id}: failed to read current HEAD"))?;
    let repo = active
        .bundle
        .repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .with_context(|| format!("{repo_id}: repo is no longer tracked in this bundle"))?;

    Ok(RepoRevertPlan {
        repo_id: repo_id.to_string(),
        worktree_path: repo
            .worktree_path
            .clone()
            .unwrap_or_else(|| worktree.display().to_string()),
        expected_head_sha: Some(expected_head_sha),
        operations,
    })
}

fn pr_revert_repo_plan(
    active: &ActiveBundle,
    repo_id: &str,
    selector: &str,
) -> Result<RepoRevertPlan> {
    let repo = active
        .bundle
        .repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .with_context(|| format!("{repo_id}: repo is no longer tracked in this bundle"))?;
    let worktree_path = repo
        .worktree_path
        .clone()
        .or_else(|| checkout_dir(active, repo).map(|path| path.display().to_string()))
        .unwrap_or_else(|| repo.path.clone());

    Ok(RepoRevertPlan {
        repo_id: repo_id.to_string(),
        worktree_path,
        expected_head_sha: None,
        operations: vec![RevertOperation {
            kind: RevertOpKind::PrRevert,
            sha: None,
            selector: Some(selector.to_string()),
        }],
    })
}

fn preflight_plan(active: &ActiveBundle, plan: &RevertPlan) -> Result<()> {
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
    if !matches!(operation.kind, RevertOpKind::Revert | RevertOpKind::CherryPick) {
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

    let (_, target, forge) =
        provider_revert_context(active, plan.provider.as_deref(), &repo_plan.repo_id)?;
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

fn plan_uses_provider_revert(plan: &RevertPlan) -> bool {
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
    let target = provider_target(active, repo, forge.as_ref())?;

    Ok((index, target, forge))
}

fn provider_target(
    active: &ActiveBundle,
    repo: &crate::model::RepoEntry,
    forge: &dyn providers::Forge,
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
    bail!(
        "{}: no checkout or parseable remote is available for provider PR revert.",
        repo.id
    );
}

fn pull_request_is_merged(pr: &PullRequest) -> bool {
    pr.state.as_deref() == Some("MERGED")
}

fn repo_context(active: &ActiveBundle, repo_id: &str) -> Result<(usize, PathBuf)> {
    let (index, repo) = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .find(|(_, repo)| repo.id == repo_id)
        .with_context(|| format!("{repo_id}: repo is no longer tracked in this bundle"))?;
    let Some(worktree) = checkout_dir(active, repo) else {
        bail!("{repo_id}: no checkout is recorded in the bundle.");
    };
    ensure_expected_branch(repo, &worktree)?;

    Ok((index, worktree))
}

fn node_message(node: &BundleNode) -> String {
    if let Some(message) = &node.message {
        return message.clone();
    }

    match node.node_type.as_str() {
        "git.observed" => "observed git changes".to_string(),
        "land.update" => "feature branch update".to_string(),
        "feature.landed" => "landed PR group".to_string(),
        "pr.revert" => "provider PR revert".to_string(),
        "repo.removed" => "removed repos".to_string(),
        node_type => node_type.to_string(),
    }
}

fn plan_path(active: &ActiveBundle, target_node_id: &str) -> PathBuf {
    active
        .root
        .join(".knit/revert-plans")
        .join(format!("{target_node_id}.json"))
}

fn print_plan(plan: &RevertPlan, path: &PathBuf) {
    println!("{} {}", out::heading("Revert plan"), out::node(&plan.id));
    println!(
        "{} {} -> {} ({})",
        out::heading("Target:"),
        plan.target_ref,
        out::node(&plan.target_node_id),
        plan.target_node_type
    );
    println!(
        "{} {}",
        out::heading("Plan file:"),
        out::path(path.display())
    );
    println!("{} {}", out::heading("Summary:"), plan.target_message);
    if let Some(provider) = &plan.provider {
        println!("{} {}", out::heading("Provider:"), provider);
    }
    println!();

    for repo in &plan.repos {
        if let Some(expected_head_sha) = &repo.expected_head_sha {
            println!(
                "{} {} {}",
                out::repo(&repo.repo_id),
                out::muted("at"),
                out::sha(short_sha(expected_head_sha))
            );
        } else {
            println!("{}", out::repo(&repo.repo_id));
        }
        for operation in &repo.operations {
            match operation.kind {
                RevertOpKind::PrRevert => println!(
                    "  {} {}",
                    out::movement(operation.kind.as_str()),
                    operation
                        .selector
                        .as_deref()
                        .unwrap_or("(missing selector)")
                ),
                _ => println!(
                    "  {} {}",
                    out::movement(operation.kind.as_str()),
                    operation
                        .sha
                        .as_deref()
                        .map(|sha| out::sha(short_sha(sha)))
                        .unwrap_or_else(|| out::danger("(missing sha)"))
                ),
            }
        }
    }

    println!();
    println!(
        "{} knit revert {} --apply",
        out::heading("Apply:"),
        plan.target_ref
    );
}
