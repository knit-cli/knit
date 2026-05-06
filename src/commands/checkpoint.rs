use crate::ids::node_id;
use crate::model::BundleNode;
use crate::output as out;
use crate::store::{load_active_bundle_for_update, save_active_bundle};
use crate::time::now_iso;
use anyhow::{bail, Result};

pub fn record_checkpoint(message: &str) -> Result<()> {
    let message = message.trim();
    if message.is_empty() {
        bail!("Checkpoint message must not be empty.");
    }

    let mut active = load_active_bundle_for_update()?;
    let now = now_iso();
    let id = node_id("cp");
    active
        .bundle
        .nodes
        .push(BundleNode::checkpoint(id.clone(), now, message.to_string()));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;

    println!("{} {}", out::heading("Recorded checkpoint"), out::node(&id));
    Ok(())
}
