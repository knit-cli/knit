//! Pull recorded bundle state from a KnitHub remote (one bundle or workspace
//! wide), list/fetch remote bundles, and delete remote bundle records.

use super::client::{
    decode_bundle_payload, ensure_remote_bundle_fast_forward, fast_forward_feature_checkouts,
    fetch_project_export, localize_bundle, load_project_if_present, prepare_feature_branches,
    request_json, resolve_remote, resolve_sync_remote_name, resolve_token, workspace_config,
};
use super::{RemoteBundle, RemoteProjectExport};
use crate::commands::worktree::materialize_repos;
use crate::model::{ChangeGroup, KnitConfig, KnitProject};
use crate::output as out;
use crate::store::{bundle_path, load_active_bundle, read_json, save_active_bundle, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

/// The local project plus the remote project export, fetched once so many
/// bundles can be localized and pulled without repeating the network round-trip.
pub struct RemotePullContext {
    project: KnitProject,
    export: RemoteProjectExport,
}

/// Outcome of pulling a single bundle's recorded state from the remote.
pub enum RemoteBundleOutcome {
    /// The artifact was applied; carries its hash.
    Pulled(String),
    /// Nothing to apply; carries a human-readable reason.
    Skipped(String),
}

/// A bundle as it exists on the configured KnitHub sync remote, with its current
/// artifact payload decoded into a `ChangeGroup` when one is present.
pub struct RemoteBundleRecord {
    pub remote_id: String,
    pub slug: String,
    pub lifecycle_state: String,
    pub payload: Option<ChangeGroup>,
}

/// Resolve the configured KnitHub remote and fetch the project export a single
/// time. Returns `None` when the pull opts out (`--no-remote`) or no remote is
/// configured, so callers can skip the artifact step without it being an error.
/// Remote resolution matches push-sync: explicit override, then `sync_remote`,
/// then a remote literally named `knithub`.
pub fn prepare_remote_pull(
    remote_override: Option<&str>,
    skip_remote: bool,
) -> Result<Option<RemotePullContext>> {
    if skip_remote {
        return Ok(None);
    }
    let (root, config) = workspace_config()?;
    let Some(remote_name) = remote_override
        .map(crate::ids::slugify)
        .or_else(|| config.sync_remote.clone())
        .or_else(|| {
            config
                .remotes
                .contains_key("knithub")
                .then(|| "knithub".to_string())
        })
    else {
        return Ok(None);
    };
    let remote = match resolve_remote(&config, &remote_name) {
        Ok(remote) => remote,
        Err(error) => {
            // An explicitly requested remote that is missing is an error; an
            // implicit fallback that is missing is simply skipped.
            if remote_override.is_some() {
                return Err(error);
            }
            return Ok(None);
        }
    };
    let token = resolve_token(&remote_name, remote)?;
    let project_id = config
        .active_project
        .clone()
        .context("No active project selected for remote pull. Run `knit project init <name>`.")?;
    let project = load_project_if_present(&root, &project_id)?
        .with_context(|| format!("No local Knit project named `{project_id}`."))?;
    let export = fetch_project_export(remote, &token, &project_id)?;
    Ok(Some(RemotePullContext { project, export }))
}

/// Pull one named bundle's recorded state from a prepared remote context:
/// localize the remote artifact, fast-forward its feature checkouts, and save.
/// Works for any bundle by id, not just the resolved one, so a workspace-wide
/// pull can process every open bundle. Callers must serialize git work that
/// touches shared source repos; this function only mutates the named bundle's
/// own artifact and checkouts.
pub fn pull_bundle_remote_state(
    root: &Path,
    context: &RemotePullContext,
    bundle_id: &str,
) -> Result<RemoteBundleOutcome> {
    let path = bundle_path(root, bundle_id);
    if !path.exists() {
        return Ok(RemoteBundleOutcome::Skipped(
            "no local bundle artifact".to_string(),
        ));
    }
    let local: ChangeGroup = read_json(&path)?;
    let Some(remote_bundle) = context
        .export
        .bundles
        .iter()
        .find(|bundle| bundle.slug == bundle_id)
    else {
        return Ok(RemoteBundleOutcome::Skipped("not present on remote".to_string()));
    };
    let Some(artifact) = remote_bundle.current_artifact.as_ref() else {
        return Ok(RemoteBundleOutcome::Skipped("no remote artifact".to_string()));
    };
    let remote_payload = decode_bundle_payload(&artifact.payload, &remote_bundle.slug)?;
    let localized = localize_bundle(remote_payload, &context.project)?;
    prepare_feature_branches(&localized)?;
    ensure_remote_bundle_fast_forward(&local, &localized)?;
    let mut active = ActiveBundle::unlocked(root.to_path_buf(), path, localized);
    materialize_repos(&mut active, None)?;
    fast_forward_feature_checkouts(&mut active)?;
    save_active_bundle(&active)?;
    Ok(RemoteBundleOutcome::Pulled(artifact.artifact_hash.clone()))
}

pub fn pull_remote_state(remote_name: Option<&str>, skip_remote: bool) -> Result<()> {
    let Some(context) = prepare_remote_pull(remote_name, skip_remote)? else {
        return Ok(());
    };
    let active = load_active_bundle()?;
    match pull_bundle_remote_state(&active.root, &context, &active.bundle.id)? {
        RemoteBundleOutcome::Pulled(hash) => println!(
            "{} {} {}",
            out::movement("pulled"),
            out::repo(&active.bundle.id),
            out::muted(&hash)
        ),
        RemoteBundleOutcome::Skipped(reason) => println!(
            "{} {}",
            out::warn("KnitHub pull skipped:"),
            out::muted(reason)
        ),
    }
    Ok(())
}

/// List the bundle records the sync remote holds for `project_id`, decoding each
/// bundle's current artifact payload when it is a supported Knit bundle.
pub fn list_remote_bundles(config: &KnitConfig, project_id: &str) -> Result<Vec<RemoteBundleRecord>> {
    let remote_name = resolve_sync_remote_name(config)?;
    let remote = resolve_remote(config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;
    let export = fetch_project_export(remote, &token, project_id)?;
    Ok(export
        .bundles
        .into_iter()
        .map(|bundle| {
            let payload = bundle
                .current_artifact
                .as_ref()
                .and_then(|artifact| decode_bundle_payload(&artifact.payload, &bundle.slug).ok());
            RemoteBundleRecord {
                remote_id: bundle.id,
                slug: bundle.slug,
                lifecycle_state: bundle.lifecycle_state,
                payload,
            }
        })
        .collect())
}

/// Delete a single bundle record from the sync remote by its remote id, returning the
/// deleted bundle's slug.
pub fn delete_remote_bundle_by_id(config: &KnitConfig, remote_id: &str) -> Result<String> {
    let remote_name = resolve_sync_remote_name(config)?;
    let remote = resolve_remote(config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;
    let deleted: RemoteBundle =
        request_json(remote, &token, "DELETE", &format!("/bundles/{remote_id}"), None)?;
    Ok(deleted.slug)
}

pub fn delete_bundle_from_remote(
    _root: &Path,
    config: &KnitConfig,
    bundle: &ChangeGroup,
) -> Result<()> {
    let remote_name = resolve_sync_remote_name(config)?;
    let remote = resolve_remote(config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;
    let project_id = bundle
        .project_id
        .clone()
        .or_else(|| config.active_project.clone())
        .context("No project selected for remote bundle cleanup. Set activeProject or record projectId on the bundle.")?;
    let export = fetch_project_export(remote, &token, &project_id)?;
    let Some(remote_bundle) = export.bundles.iter().find(|remote_bundle| {
        remote_bundle.slug == bundle.id && remote_bundle.lifecycle_state != "deleted"
    }) else {
        println!(
            "{}: {}",
            out::node(&bundle.id),
            out::muted("remote bundle already missing")
        );
        return Ok(());
    };

    let deleted: RemoteBundle = request_json(
        remote,
        &token,
        "DELETE",
        &format!("/bundles/{}", remote_bundle.id),
        None,
    )?;
    println!(
        "{}: {} {}",
        out::node(&bundle.id),
        out::movement("deleted remote bundle"),
        out::muted(format!("{remote_name}/{}", deleted.slug))
    );
    Ok(())
}

pub fn fetch_bundles_from_remote(
    root: &Path,
    config: &KnitConfig,
    remote_name: Option<&str>,
) -> Result<()> {
    let remote_name = remote_name.map(crate::ids::slugify).or(config.sync_remote.clone()).with_context(|| {
        "No remote configured for bundle fetch. Configure a remote with `knit remote add` or set sync_remote."
    })?;
    let remote = resolve_remote(config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;

    let project_id = config
        .active_project
        .clone()
        .context("Bundle fetch requires active_project. Set with `knit project init <name>`.")?;

    let export = fetch_project_export(remote, &token, &project_id)?;

    let Some(local_project) = load_project_if_present(root, &project_id)? else {
        bail!("No local project `{project_id}` found. Cannot localize bundles.");
    };

    let bundles_dir = root.join(".knit/bundles");
    fs::create_dir_all(&bundles_dir)
        .with_context(|| format!("failed to create bundles directory {}", bundles_dir.display()))?;

    let mut fetched_count = 0;
    for remote_bundle in export.bundles {
        if remote_bundle.lifecycle_state == "deleted" {
            continue;
        }
        let Some(artifact) = remote_bundle.current_artifact.as_ref() else {
            continue;
        };

        let mut bundle = decode_bundle_payload(&artifact.payload, &remote_bundle.slug)
            .with_context(|| format!("failed to decode bundle `{}`", remote_bundle.slug))?;
        bundle = localize_bundle(bundle, &local_project)
            .with_context(|| format!("failed to localize bundle `{}`", remote_bundle.slug))?;

        let bundle_path = bundles_dir.join(format!("{}.bundle.json", bundle.id));
        crate::store::write_json(&bundle_path, &bundle)
            .with_context(|| format!("failed to write bundle `{}`", remote_bundle.slug))?;
        fetched_count += 1;
    }

    if fetched_count > 0 {
        println!(
            "{} {} bundle(s) from {}",
            out::movement("fetched"),
            out::ok(fetched_count),
            out::repo(&remote_name)
        );
    } else {
        println!("{} no bundles to fetch from {}", out::muted("already up-to-date"), out::repo(&remote_name));
    }
    Ok(())
}
