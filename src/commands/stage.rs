use crate::checkout::{checkout_dir, ensure_expected_branch};
use crate::commands::commit::stage_all_tracked;
use crate::git::git_output;
use crate::output as out;
use crate::status::status_label;
use crate::store::load_active_bundle_for_update;
use anyhow::Result;

pub fn stage_all() -> Result<()> {
    let active = load_active_bundle_for_update()?;
    stage_all_tracked(&active)?;

    for repo in &active.bundle.repos {
        let Some(worktree_abs) = checkout_dir(&active, repo) else {
            println!("{}: {}", out::repo(&repo.id), out::muted("no worktree"));
            continue;
        };
        ensure_expected_branch(repo, &worktree_abs)?;
        let short_status = git_output(&worktree_abs, ["status", "--short"])?;
        println!(
            "{}: {}",
            out::repo(&repo.id),
            out::status(status_label(&short_status))
        );
    }

    Ok(())
}
