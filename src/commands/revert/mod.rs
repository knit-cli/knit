//! `knit revert` — append-only undo. Builds a revert plan from a ledger
//! node ([`plan`]) and executes it ([`apply`]): local revert/cherry-pick
//! commits, or provider PR reverts for landed nodes.

mod apply;
mod plan;

use crate::checkout::{checkout_dir, ensure_expected_branch};
use crate::ids::short_sha;
use crate::model::BundleNode;
use crate::output as out;
use crate::selectors::resolve_log_node;
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, read_json, write_json, ActiveBundle,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use apply::{apply_local_revert, apply_provider_revert, plan_uses_provider_revert, preflight_plan};
use plan::build_plan;

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
