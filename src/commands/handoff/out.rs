use super::*;
use crate::{
    commands::{
        commit::commit_active,
        remote::{configured_sync_remote_names, push_active_bundle_to_remote},
    },
    model::{BundleNode, BundleState, NodeHandoff},
    store::{read_json, save_active_bundle, write_json, ActiveBundle},
    time::now_iso,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutJournal {
    id: String,
    node_id: String,
    destination: Option<String>,
    message: Option<String>,
    source: NodeOrigin,
    checkpoint_commit_group_ids: Vec<String>,
    published: bool,
    previous_handoff: Option<String>,
}

pub fn handoff_out(to: Option<&str>, message: Option<&str>, force: bool, json: bool) -> Result<()> {
    if json {
        crate::output::route_human_lines_to_stderr();
    }
    let mut active = store::load_active_bundle_for_update()?;
    if !matches!(active.bundle.state, None | Some(BundleState::Open)) {
        bail!("Only open bundles can be handed off");
    }
    let config = store::load_effective_config(&active.root)?;
    let remote = configured_sync_remote_names(&config)
        .into_iter()
        .next()
        .context("Configure a sync remote before handing off")?;
    let project_ref = active
        .bundle
        .project_id
        .clone()
        .context("Bundle must belong to a project")?;
    let journal_path = active
        .root
        .join(".knit/handoffs")
        .join(format!("{}.out.json", active.bundle.id));
    std::fs::create_dir_all(
        journal_path
            .parent()
            .context("Handoff journal has no parent")?,
    )?;
    let existing: Option<OutJournal> = journal_path
        .exists()
        .then(|| read_json(&journal_path))
        .transpose()?;
    let mut journal = if let Some(j) = existing.filter(|j| !j.published) {
        if !same_origin(&j.source, &ambient_origin()) {
            bail!("Unfinished handoff belongs to a different source environment");
        }
        if to.is_some() && to != j.destination.as_deref() {
            bail!("Retry the unfinished handoff to its original destination");
        }
        j
    } else {
        if let Some(location) = active.bundle.handoff_location() {
            if location.state == HandoffLocationState::Conflict {
                bail!("Resolve competing handoffs before publishing another handoff");
            }

            if !force
                && (location.state != HandoffLocationState::Active
                    || !location
                        .origin
                        .as_ref()
                        .is_some_and(|o| same_origin(o, &ambient_origin())))
            {
                bail!("Bundle already handed off to {}; use handoff in to continue here, or --force to publish a new handoff", location.label);
            }
        }
        OutJournal {
            id: crate::ids::node_id("handoff"),
            node_id: crate::ids::node_id("hout"),
            destination: to.map(str::to_owned),
            message: message.map(str::to_owned),
            source: ambient_origin(),
            checkpoint_commit_group_ids: vec![],
            published: false,
            previous_handoff: active.bundle.handoff_location().and_then(|location| {
                let kind = if location.state == HandoffLocationState::Active {
                    "handoff.in"
                } else {
                    "handoff.out"
                };
                active
                    .bundle
                    .nodes
                    .iter()
                    .find(|n| {
                        n.node_type == kind
                            && n.handoff
                                .as_ref()
                                .is_some_and(|h| h.id == location.handoff_id)
                    })
                    .map(|n| n.id.clone())
            }),
        }
    };
    let mut dirty = false;
    for repo in &active.bundle.repos {
        let cwd = crate::checkout::checkout_dir(&active, repo)
            .with_context(|| format!("{} has no materialized checkout", repo.id))?;
        if crate::git::current_branch(&cwd)?.as_deref() != repo.feature_branch.as_deref() {
            bail!("{} is not on its recorded feature branch", repo.id);
        }
        if repo.remote.is_none() {
            bail!("{} has no Git remote; handoff cannot transport it", repo.id);
        }
        check_git_operation(&cwd)?;
        dirty |= crate::pending::path_pending_changes(&cwd)?.any();
        // Dirty submodules cannot be transported by committing their parent.
        let submodules = crate::git::git_output(
            &cwd,
            ["submodule", "foreach", "--quiet", "git status --porcelain"],
        )?;
        if !submodules.trim().is_empty() {
            bail!(
                "{} has dirty submodules; commit and push those repositories first",
                repo.id
            );
        }
    }
    if let Some(index) = active
        .bundle
        .nodes
        .iter()
        .position(|n| n.id == journal.node_id)
    {
        let newer_recorded_work = active.bundle.nodes[index + 1..]
            .iter()
            .any(|n| !n.commits.is_empty() || !n.repo_changes.is_empty());
        if dirty
            || newer_recorded_work
            || !crate::tracking::detect_unrecorded_changes(&active)?.is_empty()
        {
            bail!("The prepared handoff snapshot has newer local changes. Preserve those changes separately before retrying this publication; its recorded checkpoint is immutable.");
        }
    }
    write_json(&journal_path, &journal)?;
    if dirty {
        let before: BTreeSet<_> = active
            .bundle
            .commit_groups
            .iter()
            .map(|g| g.id.clone())
            .collect();
        let checkpoint_message = format!("knit handoff checkpoint: {}", active.bundle.id);
        let result = commit_active(&mut active, &checkpoint_message, true);
        for group in active
            .bundle
            .commit_groups
            .clone()
            .into_iter()
            .filter(|g| !before.contains(&g.id))
        {
            journal.checkpoint_commit_group_ids.push(group.id.clone());
            active.bundle.nodes.push(BundleNode::checkpoint(
                crate::ids::node_id("checkpoint"),
                now_iso(),
                checkpoint_message.clone(),
                group.commits.iter().map(|c| c.repo_id.clone()).collect(),
                group.id,
            ));
        }
        active.bundle.head_node_id = active.bundle.nodes.last().map(|n| n.id.clone());
        save_active_bundle(&active)?;
        write_json(&journal_path, &journal)?;
        result?;
    }
    let observed = crate::tracking::sync_observed_changes(&mut active)?;
    if !observed.is_empty() {
        save_active_bundle(&active)?;
    }
    if !active.bundle.nodes.iter().any(|n| n.id == journal.node_id) {
        // Publish every checkpoint and branch before announcing the handoff.
        push_active_bundle_to_remote(&remote, None, &mut active, crate::commands::PushForce::No)?;
        let payload = NodeHandoff {
            id: journal.id.clone(),
            source: journal.source.clone(),
            destination: journal.destination.clone(),
            checkpoint_commit_group_ids: journal.checkpoint_commit_group_ids.clone(),
            size_mib: Some(measure_size(&active)?),
            out_node_id: None,
        };
        let mut node = BundleNode::handoff_out(
            journal.node_id.clone(),
            now_iso(),
            journal.destination.clone(),
            journal.message.clone(),
            active.bundle.repos.iter().map(|r| r.id.clone()).collect(),
            payload,
        );
        node.target_node_id = journal.previous_handoff.clone();
        active.bundle.head_node_id = Some(node.id.clone());
        active.bundle.nodes.push(node);
        active.bundle.updated_at = now_iso();
        save_active_bundle(&active)?;
    }
    push_active_bundle_to_remote(&remote, None, &mut active, crate::commands::PushForce::No)
        .context(
            "Handoff saved locally; retry handoff out to finish publishing the same handoff",
        )?;
    journal.published = true;
    write_json(&journal_path, &journal)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"handoffId":journal.id,"bundleSlug":active.bundle.id,"projectRef":project_ref,"worktreePath":active.root.join(".knit/worktrees").join(&active.bundle.id),"checkpointCommitGroupIds":journal.checkpoint_commit_group_ids})
            )?
        );
    } else {
        crate::human!(
            "Continue on the target: knit handoff in {} {}",
            project_ref,
            active.bundle.id
        );
    }
    Ok(())
}

/// Count the common object store once and tracked/unignored working files.
/// Build outputs are excluded, and linked-worktree .git pointer files are resolved.
fn measure_size(active: &ActiveBundle) -> Result<u64> {
    let mut bytes = 0u64;
    let mut git_dirs = BTreeSet::new();
    for repo in &active.bundle.repos {
        let cwd = crate::checkout::checkout_dir(active, repo).context("Missing checkout")?;
        let common = crate::git::git_output(&cwd, ["rev-parse", "--git-common-dir"])?;
        let path = PathBuf::from(common.trim());
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        let path = std::fs::canonicalize(path)?;
        if git_dirs.insert(path.clone()) {
            let size = probe::command_output(
                "du",
                &["-sk".into(), path.to_string_lossy().into_owned()],
                &cwd,
            )?;
            bytes = bytes.saturating_add(
                size.split_whitespace()
                    .next()
                    .context("du returned no size")?
                    .parse::<u64>()?
                    .saturating_mul(1024),
            );
        }
        let files = crate::git::git_output(
            &cwd,
            [
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        for file in files.split('\0').filter(|s| !s.is_empty()) {
            if let Ok(m) = std::fs::symlink_metadata(cwd.join(file)) {
                if m.is_file() || m.file_type().is_symlink() {
                    bytes = bytes.saturating_add(m.len());
                }
            }
        }
    }
    Ok(bytes.saturating_mul(13).div_ceil(10 * 1_048_576))
}

pub(super) fn published_out(active: &ActiveBundle) -> Result<Option<serde_json::Value>> {
    let path = active
        .root
        .join(".knit/handoffs")
        .join(format!("{}.out.json", active.bundle.id));
    if !path.exists() {
        return Ok(None);
    }
    let journal: OutJournal = read_json(&path)?;
    let current = active.bundle.handoff_location();
    if !journal.published
        || !current
            .is_some_and(|l| l.state == HandoffLocationState::Pending && l.handoff_id == journal.id)
    {
        return Ok(None);
    }
    Ok(Some(
        serde_json::json!({"handoffId":journal.id,"bundleSlug":active.bundle.id,"projectRef":active.bundle.project_id,"worktreePath":active.root.join(".knit/worktrees").join(&active.bundle.id),"checkpointCommitGroupIds":journal.checkpoint_commit_group_ids}),
    ))
}
