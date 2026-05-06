use crate::checkout::{checkout_dir, checkout_display_path, checkout_mode_label, is_in_place};
use crate::git::current_branch;
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
        "{} {} {} {} {}",
        out::header_field("repo", 14),
        out::header_field("branch", 26),
        out::header_field("worktree", 48),
        out::header_field("mode", 10),
        out::heading("status")
    );

    for repo in &active.bundle.repos {
        let expected_branch = repo.feature_branch.as_deref().unwrap_or("(not created)");
        let worktree = checkout_display_path(repo);
        let status_dir = checkout_dir(&active, repo).unwrap_or_else(|| PathBuf::from(&repo.path));
        let actual_branch =
            current_branch(&status_dir)?.unwrap_or_else(|| "(detached)".to_string());
        let branch = if is_in_place(repo)
            && repo.feature_branch.is_some()
            && actual_branch != expected_branch
        {
            format!("{actual_branch} != {expected_branch}")
        } else {
            expected_branch.to_string()
        };
        let short_status = git_output(&status_dir, ["status", "--short"])?;
        let mut label = status_label(&short_status).to_string();
        if is_in_place(repo) && repo.feature_branch.is_some() && actual_branch != expected_branch {
            label.push_str(" (wrong branch)");
        }
        if let Some(change) = unrecorded.iter().find(|change| change.repo_id == repo.id) {
            label.push_str(&format!(" ({})", status_note(change)));
        }
        println!(
            "{} {} {} {} {}",
            out::repo_field(&repo.id, 14),
            out::branch_field(&branch, 26),
            out::path_field(&worktree, 48),
            out::header_field(checkout_mode_label(repo), 10),
            out::status(&label)
        );
    }

    Ok(())
}
