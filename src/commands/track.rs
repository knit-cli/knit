use crate::commands::agents::{
    print_bundle_worktree_agents_summary, write_bundle_worktree_agents_md,
};
use crate::commands::base::{snapshot_base, BundleBaseMode};
use crate::commands::worktree::materialize_repos;
use crate::git::{current_branch, git_output_optional, git_root, infer_base_branch};
use crate::ids::{node_id, slugify, unique_repo_id};
use crate::model::{BundleNode, CheckoutMode, ProjectRepoEntry, RepoEntry};
use crate::output as out;
use crate::paths::same_path;
use crate::store::{load_active_bundle_for_update, save_active_bundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub struct RepoPlan {
    desired_id: String,
    path: String,
    remote: Option<String>,
    base_branch: String,
    base_sha: String,
    checkout_mode: CheckoutMode,
}

pub fn track_repos(
    repo_paths: &[PathBuf],
    base_override: Option<&str>,
    materialize: bool,
    in_place: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let checkout_mode = if in_place {
        CheckoutMode::InPlace
    } else {
        CheckoutMode::Worktree
    };
    let plans = resolve_repo_plans(
        repo_paths,
        base_override,
        checkout_mode,
        BundleBaseMode::FreshRemote,
    )?;
    apply_repo_plans(&mut active, plans, materialize)?;
    // Persist the fully materialized artifact before printing the AGENTS.md
    // summary, so a SIGPIPE on that final write can no longer drop the recorded
    // worktree metadata.
    save_active_bundle(&active)?;
    if materialize {
        let bundle_agents = write_bundle_worktree_agents_md(&active)?;
        print_bundle_worktree_agents_summary(bundle_agents.as_deref());
    }
    Ok(())
}

pub fn track_repo_selectors(
    selectors: &[String],
    base_override: Option<&str>,
    materialize: bool,
    in_place: bool,
    base_mode: BundleBaseMode,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let checkout_mode = if in_place {
        CheckoutMode::InPlace
    } else {
        CheckoutMode::Worktree
    };
    let mut plans = Vec::new();
    for selector in selectors {
        if let Some(plan) = resolve_project_selector(&active, selector, in_place, base_mode)? {
            plans.push(plan);
        } else {
            plans.push(resolve_repo_plan(
                Path::new(selector),
                base_override,
                None,
                checkout_mode,
                base_mode,
            )?);
        }
    }
    ensure_unique_paths(&plans)?;
    apply_repo_plans(&mut active, plans, materialize)?;
    // Persist the fully materialized artifact before printing the AGENTS.md
    // summary, so a SIGPIPE on that final write can no longer drop the recorded
    // worktree metadata.
    save_active_bundle(&active)?;
    if materialize {
        let bundle_agents = write_bundle_worktree_agents_md(&active)?;
        print_bundle_worktree_agents_summary(bundle_agents.as_deref());
    }
    Ok(())
}

pub fn track_project_repos(
    active: &mut crate::store::ActiveBundle,
    repos: &[ProjectRepoEntry],
    materialize: bool,
    in_place: bool,
    base_mode: BundleBaseMode,
) -> Result<()> {
    // Resolve and fetch every base before recording any repo. A failure leaves
    // the bundle empty, so start_bundle can roll it back without orphaning
    // branches or partially materialized worktrees.
    let plans = std::thread::scope(|scope| {
        let handles: Vec<_> = repos
            .iter()
            .cloned()
            .map(|repo| scope.spawn(move || resolve_project_repo_plan(&repo, in_place, base_mode)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("base snapshot worker panicked"))
            .collect::<Result<Vec<_>>>()
    })?;
    ensure_unique_paths(&plans)?;
    apply_repo_plans(active, plans, materialize)
}

fn apply_repo_plans(
    active: &mut crate::store::ActiveBundle,
    plans: Vec<RepoPlan>,
    materialize: bool,
) -> Result<()> {
    let mut touched_repo_ids = Vec::new();

    for plan in plans {
        if let Some(index) = active
            .bundle
            .repos
            .iter()
            .position(|repo| same_path(&repo.path, &plan.path))
        {
            let existing = &mut active.bundle.repos[index];
            existing.remote = plan.remote;
            existing.base_branch = plan.base_branch;
            existing.base_sha = Some(plan.base_sha);
            existing.checkout_mode = plan.checkout_mode;
            touched_repo_ids.push(existing.id.clone());
            println!(
                "{} {} ({})",
                out::movement("updated"),
                out::repo(&existing.id),
                out::path(&existing.path)
            );
            continue;
        }

        let repo_id = unique_repo_id(&active.bundle, &plan.desired_id);
        active.bundle.repos.push(RepoEntry {
            id: repo_id.clone(),
            path: plan.path,
            remote: plan.remote,
            base_branch: plan.base_branch,
            checkout_mode: plan.checkout_mode,
            base_sha: Some(plan.base_sha),
            feature_branch: None,
            worktree_path: None,
            head_sha: None,
        });
        println!("{} {}", out::movement("added"), out::repo(&repo_id));
        touched_repo_ids.push(repo_id);
    }

    if touched_repo_ids.is_empty() {
        return Ok(());
    }

    let now = now_iso();
    active.bundle.nodes.push(BundleNode::repos_added(
        node_id("repo"),
        now.clone(),
        touched_repo_ids.clone(),
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();

    if !materialize {
        return Ok(());
    }

    // Crash-consistency: persist the recorded repo entries before creating any
    // git side effects. If materialization is interrupted (a SIGPIPE from a
    // closed output pipe, a Ctrl-C, or a crash), the saved artifact already lists
    // the repo, so the branch and worktree it creates are never orphaned —
    // `knit bundle worktree` rematerializes the missing checkout. Saving only
    // after materialization (the previous order) could leave a branch and
    // worktree the bundle artifact never recorded, which later commit/publish
    // then silently skipped.
    save_active_bundle(active)?;

    let materialized_repo_ids = materialize_repos(active, Some(&touched_repo_ids))?;
    let now = now_iso();
    active.bundle.nodes.push(BundleNode::worktrees_materialized(
        node_id("worktree"),
        now,
        materialized_repo_ids,
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();

    Ok(())
}

fn resolve_repo_plans(
    repo_paths: &[PathBuf],
    base_override: Option<&str>,
    checkout_mode: CheckoutMode,
    base_mode: BundleBaseMode,
) -> Result<Vec<RepoPlan>> {
    let mut plans: Vec<RepoPlan> = Vec::new();

    for repo_path in repo_paths {
        let plan = resolve_repo_plan(repo_path, base_override, None, checkout_mode, base_mode)?;
        plans.push(plan);
    }

    ensure_unique_paths(&plans)?;
    Ok(plans)
}

fn resolve_repo_plan(
    repo_path: &Path,
    base_override: Option<&str>,
    desired_id: Option<String>,
    checkout_mode: CheckoutMode,
    base_mode: BundleBaseMode,
) -> Result<RepoPlan> {
    let repo_root = git_root(repo_path)?;
    let name = repo_root
        .file_name()
        .and_then(OsStr::to_str)
        .context("repo path has no valid final component")?
        .to_string();
    let path = repo_root.to_string_lossy().to_string();
    let current_branch = current_branch(&repo_root)?;
    let remote = git_output_optional(&repo_root, ["remote", "get-url", "origin"])?;
    let base_branch = match base_override {
        Some(base) => base.to_string(),
        None => infer_base_branch(&repo_root, current_branch.as_deref())?,
    };
    let base_sha = snapshot_base(&repo_root, &base_branch, base_mode)?.sha;

    Ok(RepoPlan {
        desired_id: desired_id.unwrap_or_else(|| slugify(&name)),
        path,
        remote,
        base_branch,
        base_sha,
        checkout_mode,
    })
}

fn resolve_project_selector(
    active: &crate::store::ActiveBundle,
    selector: &str,
    in_place: bool,
    base_mode: BundleBaseMode,
) -> Result<Option<RepoPlan>> {
    let Some(project_id) = &active.bundle.project_id else {
        return Ok(None);
    };
    let project_path = active
        .root
        .join(".knit/projects")
        .join(format!("{project_id}.project.json"));
    if !project_path.exists() {
        return Ok(None);
    }
    let project: crate::model::KnitProject = crate::store::read_json(&project_path)?;
    let Some(repo) = project.repos.iter().find(|repo| repo.id == selector) else {
        return Ok(None);
    };
    Ok(Some(resolve_project_repo_plan(repo, in_place, base_mode)?))
}

fn resolve_project_repo_plan(
    repo: &ProjectRepoEntry,
    in_place: bool,
    base_mode: BundleBaseMode,
) -> Result<RepoPlan> {
    let checkout_mode = if in_place {
        CheckoutMode::InPlace
    } else {
        repo.checkout_mode
    };
    let repo_root = git_root(Path::new(&repo.path))?;
    let snapshot = snapshot_base(&repo_root, &repo.base_branch, base_mode)
        .with_context(|| format!("{}: failed to snapshot configured base", repo.id))?;
    println!(
        "{}: base {} {}",
        out::repo(&repo.id),
        out::branch(&snapshot.source_ref),
        out::sha(crate::ids::short_sha(&snapshot.sha))
    );
    Ok(RepoPlan {
        desired_id: repo.id.clone(),
        path: repo_root.to_string_lossy().to_string(),
        remote: repo.remote.clone(),
        base_branch: repo.base_branch.clone(),
        base_sha: snapshot.sha,
        checkout_mode,
    })
}

fn ensure_unique_paths(plans: &[RepoPlan]) -> Result<()> {
    for (index, plan) in plans.iter().enumerate() {
        if plans
            .iter()
            .skip(index + 1)
            .any(|existing| same_path(&existing.path, &plan.path))
        {
            bail!("Repo {} was provided more than once.", plan.path);
        }
    }
    Ok(())
}
