use crate::git::git_output;
use crate::ids::short_sha;
use crate::store::load_active_bundle;
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;

pub fn show_log() -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.commit_groups.is_empty() {
        println!("No commit groups recorded yet.");
        return Ok(());
    }

    for group in &active.bundle.commit_groups {
        println!("{}  {}", group.id, group.message);
        for commit in &group.commits {
            println!("  {:<10} {}", commit.repo_id, short_sha(&commit.sha));
        }
    }

    Ok(())
}

pub fn show_group(commit_group_id: &str) -> Result<()> {
    let active = load_active_bundle()?;
    let group = active
        .bundle
        .commit_groups
        .iter()
        .find(|group| group.id == commit_group_id)
        .with_context(|| format!("No commit group found for {commit_group_id}"))?;

    println!("{}  {}\n", group.id, group.message);
    for commit in &group.commits {
        let repo = active
            .bundle
            .repos
            .iter()
            .find(|repo| repo.id == commit.repo_id)
            .with_context(|| format!("No repo found for {}", commit.repo_id))?;
        let repo_dir = repo
            .worktree_path
            .as_ref()
            .map(|path| active.root.join(path))
            .filter(|path| path.exists())
            .unwrap_or_else(|| PathBuf::from(&repo.path));
        println!("== {} {} ==", commit.repo_id, short_sha(&commit.sha));
        let output = git_output(
            &repo_dir,
            [
                OsString::from("show"),
                OsString::from("--stat"),
                OsString::from("--oneline"),
                OsString::from(&commit.sha),
            ],
        )?;
        println!("{output}");
    }

    Ok(())
}
