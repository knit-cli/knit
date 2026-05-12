use crate::advice;
use crate::ids::node_id;
use crate::model::{BundleNode, BUNDLE_STATE_ARCHIVED, BUNDLE_STATE_CLOSED};
use crate::output as out;
use crate::store::{load_active_bundle_for_update, save_active_bundle};
use crate::time::now_iso;
use anyhow::{bail, Result};

pub fn close_bundle(reason: Option<&str>) -> Result<()> {
    let reason = normalize_reason(reason)?;
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.state.as_deref() == Some(BUNDLE_STATE_ARCHIVED) {
        bail!(
            "Bundle {} is archived. Restore it before closing.",
            active.bundle.id
        );
    }

    if active.bundle.state.as_deref() == Some(BUNDLE_STATE_CLOSED)
        || active
            .bundle
            .nodes
            .last()
            .is_some_and(|node| node.node_type == "feature.closed")
    {
        bail!("Bundle {} is already closed.", active.bundle.id);
    }

    let now = now_iso();
    let id = node_id("close");
    active.bundle.state = Some(BUNDLE_STATE_CLOSED.to_string());
    active.bundle.closed_at = Some(now.clone());
    active
        .bundle
        .nodes
        .push(BundleNode::feature_closed(id.clone(), now, reason));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;

    println!(
        "{} {}",
        out::heading("Closed bundle"),
        out::node(&active.bundle.id)
    );
    println!("{} {}", out::heading("Node:"), out::node(&id));
    println!(
        "{} {}",
        out::heading("Preserved:"),
        "worktrees and local feature branches"
    );
    advice::print(
        &active.root,
        format!(
            "to leave a clean local workspace, run `knit bundle delete {} --force --worktrees --branches` (add `--force-branches` if the branches are not merged locally).",
            active.bundle.id
        ),
    );
    Ok(())
}

fn normalize_reason(reason: Option<&str>) -> Result<Option<String>> {
    let Some(reason) = reason else {
        return Ok(None);
    };
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("Close reason must not be empty when --reason is passed.");
    }
    Ok(Some(reason.to_string()))
}
