use crate::store::{load_active_bundle_for_update, save_active_bundle};
use crate::tracking::{sync_note, sync_observed_changes};
use anyhow::Result;

pub fn sync_bundle() -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let changes = sync_observed_changes(&mut active)?;

    if changes.is_empty() {
        println!("No unrecorded git commits found.");
        return Ok(());
    }

    for change in &changes {
        println!("{}: {}", change.repo_id, sync_note(change));
    }

    save_active_bundle(&active)?;
    Ok(())
}
