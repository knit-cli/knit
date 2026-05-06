use crate::checkout::ensure_mutable_checkouts;
use crate::output as out;
use crate::store::{load_active_bundle_for_update, save_active_bundle};
use crate::tracking::{sync_note, sync_observed_changes};
use anyhow::Result;

pub fn sync_bundle() -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    ensure_mutable_checkouts(&active)?;
    let changes = sync_observed_changes(&mut active)?;

    if changes.is_empty() {
        println!("{}", out::ok("No unrecorded git commits found."));
        return Ok(());
    }

    for change in &changes {
        println!(
            "{}: {}",
            out::repo(&change.repo_id),
            out::warn(sync_note(change))
        );
    }

    save_active_bundle(&active)?;
    Ok(())
}
