//! Build revert plans from a bundle ledger node: per-repo revert/cherry-pick
//! operations for commit groups and observed git movement, and provider PR
//! reverts for landed nodes.

use super::*;
use crate::checkout::checkout_dir;
use crate::git::rev_parse;
use crate::ids::revert_plan_id;
use crate::model::{BundleNode, CommitRef, SCHEMA_VERSION};
use crate::providers::publication_for_repo;
use crate::selectors::resolve_log_node;
use crate::store::ActiveBundle;
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn build_plan(active: &ActiveBundle, target: &str) -> Result<RevertPlan> {
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
