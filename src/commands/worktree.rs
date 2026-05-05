use crate::git::{branch_exists, git_output, is_git_worktree, resolve_base_ref};
use crate::ids::node_id;
use crate::model::BundleNode;
use crate::store::{load_active_bundle, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

pub fn create_worktrees() -> Result<()> {
    let mut active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The active bundle has no repos. Run `knit add <repo-path>` first.");
    }

    let materialized_repo_ids = materialize_repos(&mut active, None)?;
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
        let worktree_path = format!(".knit/worktrees/{}/{}", bundle_id, repo.id);
        let worktree_abs = active.root.join(&worktree_path);

        repo.feature_branch = Some(feature_branch.clone());
        repo.worktree_path = Some(worktree_path.clone());

        if worktree_abs.exists() {
            if is_git_worktree(&worktree_abs) {
                println!("{}: worktree already present at {}", repo.id, worktree_path);
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
            println!("{}: created worktree from existing branch", repo.id);
            materialized_repo_ids.push(repo.id.clone());
        } else {
            let base_ref = resolve_base_ref(&repo_root, &repo.base_branch);
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
            println!("{}: created {}", repo.id, feature_branch);
            materialized_repo_ids.push(repo.id.clone());
        }
    }

    Ok(materialized_repo_ids)
}
