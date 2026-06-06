use crate::checkout::is_in_place;
use crate::commands::agents::{
    print_bundle_worktree_agents_summary, print_worktree_agents_summary,
    write_bundle_worktree_agents_md, write_worktree_agents_md,
};
use crate::git::{
    branch_exists, current_branch, git_output, is_git_worktree, resolve_base_ref, rev_parse,
};
use crate::ids::node_id;
use crate::model::BundleNode;
use crate::output as out;
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

pub fn create_worktrees() -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let materialized_repo_ids = materialize_repos(&mut active, None)?;
    let bundle_agents = write_bundle_worktree_agents_md(&active)?;
    let worktree_agents = write_worktree_agents_md(&active)?;
    print_bundle_worktree_agents_summary(bundle_agents.as_deref());
    print_worktree_agents_summary(&worktree_agents);
    let now = now_iso();
    active.bundle.nodes.push(BundleNode::worktrees_materialized(
        node_id("worktree"),
        now,
        materialized_repo_ids,
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;
    Ok(())
}

pub fn materialize_repos(
    active: &mut ActiveBundle,
    only_repo_ids: Option<&[String]>,
) -> Result<Vec<String>> {
    let bundle_id = active.bundle.id.clone();
    fs::create_dir_all(active.root.join(".knit/worktrees").join(&bundle_id))
        .context("failed to create bundle worktree directory")?;
    let mut materialized_repo_ids = Vec::new();

    for repo in &mut active.bundle.repos {
        if let Some(repo_ids) = only_repo_ids {
            if !repo_ids.iter().any(|repo_id| repo_id == &repo.id) {
                continue;
            }
        }

        let repo_root = PathBuf::from(&repo.path);
        let feature_branch = format!("knit/{bundle_id}");
        if is_in_place(repo) {
            materialize_in_place(repo, &repo_root, &feature_branch)?;
            materialized_repo_ids.push(repo.id.clone());
            continue;
        }

        let worktree_path = format!(".knit/worktrees/{}/{}", bundle_id, repo.id);
        let worktree_abs = active.root.join(&worktree_path);
        let base_ref = resolve_base_ref(&repo_root, &repo.base_branch);
        let base_sha = rev_parse(&repo_root, &base_ref)
            .with_context(|| format!("{}: failed to resolve base ref {base_ref}", repo.id))?;

        if repo.base_sha.is_none() {
            repo.base_sha = Some(base_sha.clone());
        }
        repo.feature_branch = Some(feature_branch.clone());
        repo.worktree_path = Some(worktree_path.clone());

        if worktree_abs.exists() {
            if is_git_worktree(&worktree_abs) {
                if repo.head_sha.is_none() {
                    repo.head_sha =
                        Some(rev_parse(&worktree_abs, "HEAD").with_context(|| {
                            format!("{}: failed to read worktree HEAD", repo.id)
                        })?);
                }
                println!(
                    "{}: worktree already present at {}",
                    out::repo(&repo.id),
                    out::path(&worktree_path)
                );
                materialized_repo_ids.push(repo.id.clone());
                continue;
            }
            bail!(
                "{}: {} exists but is not a git worktree",
                repo.id,
                worktree_abs.display()
            );
        }

        if let Some(parent) = worktree_abs.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create worktree parent {}", parent.display())
            })?;
        }

        if branch_exists(&repo_root, &feature_branch) {
            git_output(
                &repo_root,
                [
                    OsString::from("worktree"),
                    OsString::from("add"),
                    worktree_abs.as_os_str().to_os_string(),
                    OsString::from(&feature_branch),
                ],
            )
            .with_context(|| format!("failed to add worktree for {}", repo.id))?;
            if repo.head_sha.is_none() {
                repo.head_sha = Some(
                    rev_parse(&worktree_abs, "HEAD")
                        .with_context(|| format!("{}: failed to read worktree HEAD", repo.id))?,
                );
            }
            println!(
                "{}: {} worktree from existing branch",
                out::repo(&repo.id),
                out::movement("created")
            );
            materialized_repo_ids.push(repo.id.clone());
        } else {
            git_output(
                &repo_root,
                [
                    OsString::from("worktree"),
                    OsString::from("add"),
                    OsString::from("-b"),
                    OsString::from(&feature_branch),
                    worktree_abs.as_os_str().to_os_string(),
                    OsString::from(base_ref),
                ],
            )
            .with_context(|| format!("failed to create branch/worktree for {}", repo.id))?;
            repo.head_sha = Some(
                rev_parse(&worktree_abs, "HEAD")
                    .with_context(|| format!("{}: failed to read worktree HEAD", repo.id))?,
            );
            println!(
                "{}: {} {}",
                out::repo(&repo.id),
                out::movement("created"),
                out::branch(feature_branch)
            );
            materialized_repo_ids.push(repo.id.clone());
        }
    }

    Ok(materialized_repo_ids)
}

fn materialize_in_place(
    repo: &mut crate::model::RepoEntry,
    repo_root: &PathBuf,
    feature_branch: &str,
) -> Result<()> {
    let base_ref = resolve_base_ref(repo_root, &repo.base_branch);
    let base_sha = rev_parse(repo_root, &base_ref)
        .with_context(|| format!("{}: failed to resolve base ref {base_ref}", repo.id))?;
    if repo.base_sha.is_none() {
        repo.base_sha = Some(base_sha);
    }

    let current_branch = current_branch(repo_root)?;
    if current_branch.as_deref() != Some(feature_branch) {
        let short_status = git_output(repo_root, ["status", "--short"])?;
        if !short_status.trim().is_empty() {
            bail!(
                "{}: in-place checkout must be clean before switching branches.",
                repo.id
            );
        }

        if branch_exists(repo_root, feature_branch) {
            git_output(repo_root, ["checkout", feature_branch])
                .with_context(|| format!("{}: failed to checkout {feature_branch}", repo.id))?;
        } else {
            git_output(
                repo_root,
                [
                    OsString::from("checkout"),
                    OsString::from("-b"),
                    OsString::from(feature_branch),
                    OsString::from(base_ref),
                ],
            )
            .with_context(|| format!("{}: failed to create {feature_branch}", repo.id))?;
        }
    }

    repo.feature_branch = Some(feature_branch.to_string());
    repo.worktree_path = Some(repo.path.clone());
    repo.head_sha = Some(
        rev_parse(repo_root, "HEAD")
            .with_context(|| format!("{}: failed to read in-place HEAD", repo.id))?,
    );
    println!(
        "{}: using in-place checkout at {}",
        out::repo(&repo.id),
        out::path(&repo.path)
    );

    Ok(())
}
