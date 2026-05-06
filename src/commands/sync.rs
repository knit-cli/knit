use crate::store::{load_active_bundle_for_update, save_active_bundle};
use crate::tracking::sync_observed_changes;
use anyhow::Result;

pub fn sync_bundle() -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let changes = sync_observed_changes(&mut active)?;

    if changes.is_empty() {
        println!("No unrecorded git commits found.");
        return Ok(());
    }

    for change in &changes {
        println!(
            "{}: observed {} unrecorded commit(s)",
            change.repo_id,
            change.commits.len()
        );
    }

    save_active_bundle(&active)?;
    Ok(())
}
