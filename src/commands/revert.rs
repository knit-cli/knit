use crate::checkout::{checkout_dir, ensure_expected_branch};
use crate::git::{git_output, rev_parse};
use crate::ids::{revert_group_id, revert_plan_id, short_sha};
use crate::model::{BundleNode, CommitGroup, CommitRef, RepoChange, SCHEMA_VERSION};
use crate::output as out;
use crate::selectors::resolve_log_node;
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, read_json, save_active_bundle, write_json,
    ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

const REVERT_PLAN_KIND: &str = "KnitRevertPlan";
const OP_REVERT: &str = "revert";
const OP_CHERRY_PICK: &str = "cherryPick";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevertPlan {
    schema_version: String,
    kind: String,
    id: String,
    bundle_id: String,
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
    expected_head_sha: String,
    operations: Vec<RevertOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevertOperation {
    kind: String,
    sha: String,
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
            "Revert plan belongs to bundle {}, but active bundle is {}.",
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
    let group_id = revert_group_id();
    let created_at = now_iso();
    let logical_message = format!("Revert {}", plan.target_message);
    let commit_message = format!(
        "{logical_message}\n\nKnit-Reverts: {}\nKnit-Group: {group_id}\nKnit-Bundle: {}",
        plan.target_node_id, active.bundle.id
    );
    let mut commits = Vec::new();
    let mut repo_changes = Vec::new();

    for repo_plan in &plan.repos {
        let (repo_index, worktree) = repo_context(&active, &repo_plan.repo_id)?;
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
            movement: "advanced".to_string(),
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
    save_active_bundle(&active)?;
    let _ = fs::remove_file(path);

    println!(
        "{} {}",
        out::heading("Recorded revert group"),
        out::node(group_id)
    );
    Ok(())
}

fn build_plan(active: &ActiveBundle, target: &str) -> Result<RevertPlan> {
    let target_node = resolve_log_node(&active.bundle.nodes, target)?;
    let mut repos = match target_node.node_type.as_str() {
        "commit.group" | "revert.group" => plans_for_commits(active, &target_node.commits)?,
        "git.observed" => plans_for_observed(active, target_node)?,
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
                    kind: OP_REVERT.to_string(),
                    sha,
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
                        kind: OP_REVERT.to_string(),
                        sha: sha.clone(),
                    }));
                }
                "rewound" => {
                    operations.extend(change.dropped_commits.iter().map(|sha| RevertOperation {
                        kind: OP_CHERRY_PICK.to_string(),
                        sha: sha.clone(),
                    }));
                }
                "diverged" => {
                    operations.extend(change.commits.iter().rev().map(|sha| RevertOperation {
                        kind: OP_REVERT.to_string(),
                        sha: sha.clone(),
                    }));
                    operations.extend(change.dropped_commits.iter().map(|sha| RevertOperation {
                        kind: OP_CHERRY_PICK.to_string(),
                        sha: sha.clone(),
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
        expected_head_sha,
        operations,
    })
}

fn preflight_plan(active: &ActiveBundle, plan: &RevertPlan) -> Result<()> {
    for repo_plan in &plan.repos {
        let (_, worktree) = repo_context(active, &repo_plan.repo_id)?;
        let current_head = rev_parse(&worktree, "HEAD")
            .with_context(|| format!("{}: failed to read current HEAD", repo_plan.repo_id))?;
        if current_head != repo_plan.expected_head_sha {
            bail!(
                "{}: HEAD changed since the revert plan was written (expected {}, found {}). Re-run `knit revert {}`.",
                repo_plan.repo_id,
                short_sha(&repo_plan.expected_head_sha),
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
    if !matches!(operation.kind.as_str(), OP_REVERT | OP_CHERRY_PICK) {
        bail!("{repo_id}: unknown revert operation `{}`.", operation.kind);
    }

    git_output(
        worktree,
        [
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from(format!("{}^{{commit}}", operation.sha)),
        ],
    )
    .with_context(|| {
        format!(
            "{repo_id}: commit {} is not available locally",
            short_sha(&operation.sha)
        )
    })?;
    Ok(())
}

fn apply_operation(worktree: &PathBuf, repo_id: &str, operation: &RevertOperation) -> Result<()> {
    match operation.kind.as_str() {
        OP_REVERT => git_output(
            worktree,
            [
                OsString::from("revert"),
                OsString::from("--no-commit"),
                OsString::from(&operation.sha),
            ],
        )
        .with_context(|| {
            format!(
                "{repo_id}: failed to revert {}. Resolve the git state manually before retrying.",
                short_sha(&operation.sha)
            )
        })?,
        OP_CHERRY_PICK => git_output(
            worktree,
            [
                OsString::from("cherry-pick"),
                OsString::from("--no-commit"),
                OsString::from(&operation.sha),
            ],
        )
        .with_context(|| {
            format!(
                "{repo_id}: failed to cherry-pick {}. Resolve the git state manually before retrying.",
                short_sha(&operation.sha)
            )
        })?,
        kind => bail!("{repo_id}: unknown revert operation `{kind}`."),
    };

    Ok(())
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
    println!();

    for repo in &plan.repos {
        println!(
            "{} {} {}",
            out::repo(&repo.repo_id),
            out::muted("at"),
            out::sha(short_sha(&repo.expected_head_sha))
        );
        for operation in &repo.operations {
            println!(
                "  {} {}",
                out::movement(operation.kind.as_str()),
                out::sha(short_sha(&operation.sha))
            );
        }
    }

    println!();
    println!(
        "{} knit revert {} --apply",
        out::heading("Apply:"),
        plan.target_ref
    );
}
