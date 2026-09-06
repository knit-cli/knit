use super::*;
use crate::{
    commands::remote::push_handoff_bundle_to_remote,
    model::{BundleNode, HandoffLocationState},
    time::now_iso,
};

pub fn handoff_in(
    project: &str,
    slug: &str,
    workspace: Option<&Path>,
    force: bool,
    json: bool,
) -> Result<()> {
    if json {
        crate::output::route_human_lines_to_stderr();
    }
    let root = target_root(project, workspace)?;
    let (report, remote) = prepare_probe(project, slug, &root, false);
    report.print(false)?;
    if report.verdict == "fail" && !force {
        return Err(report::ProbeFailed.into());
    }
    let remote = remote.context("Cannot accept without a valid authenticated remote export")?;
    // --force bypasses declared readiness only, never local work, identity,
    // schema, transport, or competing continuation checks.
    if report.checks.iter().any(|c| {
        c.status == "fail"
            && !c.id.starts_with("tool:")
            && !c.id.starts_with("env:")
            && !matches!(c.id.as_str(), "disk" | "memory" | "platform")
    }) {
        bail!("Handoff has a blocking transport or checkout failure; --force cannot bypass it");
    }
    check_existing_target(&root, &remote.bundle)?;
    let remote_name = remote.remote_name.clone();
    let location = remote
        .bundle
        .handoff_location()
        .context("Bundle has no handoff; run handoff out on the source first")?;
    if location.state == HandoffLocationState::Conflict {
        bail!("Resolve competing handoffs before accepting");
    }
    let outgoing = remote
        .bundle
        .nodes
        .iter()
        .find(|n| {
            n.node_type == "handoff.out"
                && n.handoff
                    .as_ref()
                    .is_some_and(|h| h.id == location.handoff_id)
        })
        .context("Handoff has no outgoing checkpoint")?
        .clone();
    let mut payload = outgoing
        .handoff
        .clone()
        .context("Outgoing handoff has no payload")?;
    payload.out_node_id = Some(outgoing.id);
    let mut active = remote.materialize(&root)?;
    let _lock = store::acquire_named_lock(&root, &active.bundle.id)?;
    // Reload under the acceptance lock, after materialization saved its paths.
    active.bundle = store::read_json(&active.bundle_path)?;
    let origin = ambient_origin();
    let accepted = active.bundle.nodes.iter().any(|n| {
        n.node_type == "handoff.in"
            && n.handoff.as_ref().is_some_and(|h| h.id == payload.id)
            && n.origin.as_ref().is_some_and(|o| same_origin(o, &origin))
    });
    if !accepted {
        let node = BundleNode::handoff_in(
            crate::ids::node_id("hin"),
            now_iso(),
            Some(payload.source.hostname.clone()),
            None,
            active.bundle.repos.iter().map(|r| r.id.clone()).collect(),
            payload.clone(),
        );
        active.bundle.head_node_id = Some(node.id.clone());
        active.bundle.nodes.push(node);
        active.bundle.updated_at = now_iso();
        store::save_active_bundle(&active)?;
    }
    push_handoff_bundle_to_remote(&remote_name, &mut active)
        .context("Acceptance saved locally; retry handoff in to publish it")?;
    let checkpoint_commits: Vec<_> = active
        .bundle
        .commit_groups
        .iter()
        .filter(|g| payload.checkpoint_commit_group_ids.contains(&g.id))
        .flat_map(|g| g.commits.iter().map(|c| format!("{}:{}", c.repo_id, c.sha)))
        .collect();
    let worktree = active.root.join(".knit/worktrees").join(&active.bundle.id);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"handoffId":payload.id,"bundleSlug":active.bundle.id,"bundleId":active.bundle.id,"workspaceRoot":active.root,"worktreePath":worktree,"checkpointCommits":checkpoint_commits})
            )?
        );
    } else {
        crate::human!("{}", worktree.display());
    }
    Ok(())
}
