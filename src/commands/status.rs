use crate::git::git_output;
use crate::output as out;
use crate::status::status_label;
use crate::store::load_active_bundle;
use crate::tracking::{detect_unrecorded_changes, status_note};
use anyhow::Result;
use std::path::PathBuf;

pub fn show_status() -> Result<()> {
    let active = load_active_bundle()?;
    let unrecorded = detect_unrecorded_changes(&active)?;
    println!(
        "{} {}\n",
        out::heading("Bundle:"),
        out::node(&active.bundle.id)
    );
    println!(
        "{} {} {} {}",
        out::header_field("repo", 14),
        out::header_field("branch", 26),
        out::header_field("worktree", 48),
        out::heading("status")
    );

    for repo in &active.bundle.repos {
        let branch = repo.feature_branch.as_deref().unwrap_or("(not created)");
        let worktree = repo.worktree_path.as_deref().unwrap_or("-");
        let status_dir = repo
            .worktree_path
            .as_ref()
            .map(|path| active.root.join(path))
            .filter(|path| path.exists())
            .unwrap_or_else(|| PathBuf::from(&repo.path));
        let short_status = git_output(&status_dir, ["status", "--short"])?;
        let mut label = status_label(&short_status).to_string();
        if let Some(change) = unrecorded.iter().find(|change| change.repo_id == repo.id) {
            label.push_str(&format!(" ({})", status_note(change)));
        }
        println!(
            "{} {} {} {}",
            out::repo_field(&repo.id, 14),
            out::branch_field(branch, 26),
            out::path_field(worktree, 48),
            out::status(&label)
        );
    }

    Ok(())
}
