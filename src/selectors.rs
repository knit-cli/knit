use crate::model::BundleNode;
use anyhow::{bail, Context, Result};

pub fn is_loggable_node(node: &BundleNode) -> bool {
    matches!(
        node.node_type.as_str(),
        "checkpoint" | "commit.group" | "git.observed" | "revert.group" | "repo.removed"
    )
}

pub fn resolve_log_node<'a>(nodes: &'a [BundleNode], target: &str) -> Result<&'a BundleNode> {
    let loggable = nodes
        .iter()
        .filter(|node| is_loggable_node(node))
        .collect::<Vec<_>>();
    if loggable.is_empty() {
        bail!("No bundle log entries found.");
    }

    if target == "HEAD" || target.starts_with("HEAD~") {
        let offset = if target == "HEAD" {
            0
        } else {
            target
                .strip_prefix("HEAD~")
                .and_then(|raw| raw.parse::<usize>().ok())
                .with_context(|| format!("Invalid log selector `{target}`."))?
        };
        let Some(index) = loggable.len().checked_sub(1 + offset) else {
            bail!("Log selector `{target}` is before the start of the bundle log.");
        };
        return Ok(loggable[index]);
    }

    let id_matches = loggable
        .iter()
        .filter(|node| {
            node.id == target
                || node.id.starts_with(target)
                || node
                    .commit_group_id
                    .as_deref()
                    .is_some_and(|id| id == target || id.starts_with(target))
        })
        .copied()
        .collect::<Vec<_>>();

    match id_matches.as_slice() {
        [node] => Ok(node),
        [] => resolve_log_node_by_sha(&loggable, target),
        _ => bail!("`{target}` is ambiguous; use a longer bundle node id."),
    }
}

fn resolve_log_node_by_sha<'a>(
    loggable: &[&'a BundleNode],
    target: &str,
) -> Result<&'a BundleNode> {
    let exact = matching_sha_node_indexes(loggable, target, true);
    let matches = if exact.is_empty() {
        matching_sha_node_indexes(loggable, target, false)
    } else {
        exact
    };

    let Some(index) = matches.into_iter().max() else {
        bail!("No bundle log entry or recorded git commit matched `{target}`.");
    };

    Ok(loggable[index])
}

fn matching_sha_node_indexes(loggable: &[&BundleNode], target: &str, exact: bool) -> Vec<usize> {
    loggable
        .iter()
        .enumerate()
        .filter_map(|(index, node)| node_matches_sha(node, target, exact).then_some(index))
        .collect()
}

fn node_matches_sha(node: &BundleNode, target: &str, exact: bool) -> bool {
    node.commits
        .iter()
        .any(|commit| sha_matches(&commit.sha, target, exact))
        || node.repo_changes.iter().any(|change| {
            change
                .commits
                .iter()
                .any(|sha| sha_matches(sha, target, exact))
                || change
                    .dropped_commits
                    .iter()
                    .any(|sha| sha_matches(sha, target, exact))
        })
}

fn sha_matches(sha: &str, target: &str, exact: bool) -> bool {
    if exact {
        sha == target
    } else {
        sha.starts_with(target)
    }
}
