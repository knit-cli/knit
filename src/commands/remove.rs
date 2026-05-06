use crate::ids::node_id;
use crate::model::BundleNode;
use crate::output as out;
use crate::store::{load_active_bundle_for_update, save_active_bundle};
use crate::time::now_iso;
use anyhow::{bail, Result};

pub fn remove_repos(repo_ids: &[String]) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let mut indexes = Vec::new();

    for repo_id in repo_ids {
        if repo_ids
            .iter()
            .filter(|candidate| *candidate == repo_id)
            .count()
            > 1
        {
            bail!("Repo {repo_id} was provided more than once.");
        }
        let Some(index) = active
            .bundle
            .repos
            .iter()
            .position(|repo| &repo.id == repo_id)
        else {
            bail!("Repo {repo_id} is not tracked in this bundle.");
        };
        indexes.push(index);
    }

    let mut removed = Vec::new();
    indexes.sort_unstable_by(|left, right| right.cmp(left));

    for index in indexes {
        let repo = active.bundle.repos.remove(index);
        println!(
            "{} repo {} from bundle tracking",
            out::movement("removed"),
            out::repo(&repo.id)
        );
        if let Some(worktree_path) = repo.worktree_path {
            println!(
                "{} existing worktree in place at {}",
                out::muted("Left"),
                out::path(worktree_path)
            );
        }
        removed.push(repo.id);
    }

    let now = now_iso();
    active
        .bundle
        .nodes
        .push(BundleNode::repos_removed(node_id("repo"), now, removed));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;
    Ok(())
}
