//! Pull recorded bundle state from a KnitHub remote (one bundle or workspace
//! wide), list/fetch remote bundles, and delete remote bundle records.

use super::client::{
    configured_sync_remote_names, decode_bundle_payload, effective_workspace_config,
    ensure_remote_bundle_fast_forward, fast_forward_feature_checkouts, fetch_project_export,
    load_project_if_present, localize_bundle, prepare_feature_branches, request_json,
    resolve_project_id, resolve_remote, resolve_sync_remote_name, resolve_token,
};
use super::clone::{
    clone_export_repositories, export_repo_local_id, project_repo_entry_from_export,
};
use super::{RemoteBundle, RemoteExportRepository, RemoteProjectExport, RemoteViews};
use crate::commands::worktree::materialize_repos;
use crate::model::{
    ledger_relation, merge_ledgers, ChangeGroup, KnitConfig, KnitProject, KnitProjectViews,
    KnitRemote, LedgerRelation,
};
use crate::output as out;
use crate::store::{
    acquire_named_lock, bundle_path, load_active_bundle, project_path, read_json,
    save_active_bundle, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Pull the current user's saved views for a project from the KnitHub remote,
/// replacing the local views artifact.
pub fn pull_views_from_remote(name: Option<&str>, remote_name: &str) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let project_id = resolve_project_id(&root, &config, name)?;
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let count = pull_views_into(&root, remote, &token, &project_id)?;
    println!(
        "{} {} {}",
        out::movement("pulled views"),
        out::repo(&project_id),
        out::muted(format!("{count} view(s)"))
    );
    Ok(())
}

/// Fetch a project's saved views from the remote and write the local artifact at
/// `root`, returning the number of views written. Reused by `knit clone`.
pub(super) fn pull_views_into(
    root: &Path,
    remote: &KnitRemote,
    token: &str,
    project_id: &str,
) -> Result<usize> {
    let remote_views: RemoteViews = request_json(
        remote,
        token,
        "GET",
        &format!("/projects/{project_id}/view"),
        None,
    )?;
    let mut views = KnitProjectViews::new(project_id.to_string(), now_iso());
    views.default_view = remote_views.default_view;
    views.views = remote_views.views;
    views.updated_at = now_iso();
    crate::store::save_views(root, &views)?;
    Ok(views.views.len())
}

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
    /// Diverged ledgers were union-merged into the local artifact; carries the
    /// remote artifact hash that was merged in.
    Merged(String),
    /// The artifact was already current but local checkouts were materialized
    /// and/or fast-forwarded; carries a human-readable summary.
    Refreshed(String),
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
/// Remote resolution matches the primary push-sync remote: explicit override,
/// then `syncRemotes[0]`/`sync_remote`, then the sole configured remote.
pub fn prepare_remote_pull(
    remote_override: Option<&str>,
    skip_remote: bool,
) -> Result<Option<RemotePullContext>> {
    if skip_remote {
        return Ok(None);
    }
    let (root, config) = effective_workspace_config()?;
    let Some(remote_name) = remote_override
        .map(crate::ids::slugify)
        .or_else(|| configured_sync_remote_names(&config).into_iter().next())
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
        .context("No active project selected for remote pull. Run `knit init <name>`.")?;
    let mut project = load_project_if_present(&root, &project_id)?
        .with_context(|| format!("No local Knit project named `{project_id}`."))?;
    let export = fetch_project_export(remote, &token, &project_id)?;
    crate::history::append_history_events(&root, &project_id, &export.history_events)?;
    reconcile_project_repositories(&root, &mut project, &export)?;
    Ok(Some(RemotePullContext { project, export }))
}

/// Reconcile the local project's tracked repositories with the remote export so
/// that repositories added or removed on KnitHub flow into an existing
/// workspace, not just a fresh `knit clone`. Removals drop the project repo
/// entry (the checkout on disk is left in place); additions clone the repo into
/// the workspace and record it. A degenerate export with no repositories is
/// ignored so a transient/empty response never wipes the local repo list.
fn reconcile_project_repositories(
    root: &Path,
    project: &mut KnitProject,
    export: &RemoteProjectExport,
) -> Result<()> {
    if export.repositories.is_empty() {
        return Ok(());
    }

    let export_ids: BTreeSet<String> = export
        .repositories
        .iter()
        .map(export_repo_local_id)
        .collect();

    // When the server withheld private repos from this export, an absent repo
    // is indistinguishable from a hidden one — never drop local project repos
    // on an admittedly incomplete export.
    let export_complete = export.omitted_repository_count.unwrap_or(0) == 0;
    let mut removed = Vec::new();
    if export_complete {
        project.repos.retain(|repo| {
            let keep = export_ids.contains(&repo.id);
            if !keep {
                removed.push(repo.id.clone());
            }
            keep
        });
    }

    let existing: BTreeSet<String> = project.repos.iter().map(|repo| repo.id.clone()).collect();
    let to_add: Vec<&RemoteExportRepository> = export
        .repositories
        .iter()
        .filter(|repository| !existing.contains(&export_repo_local_id(repository)))
        .collect();

    let mut added = Vec::new();
    let mut failed = Vec::new();
    for repository in to_add {
        let local_id = export_repo_local_id(repository);
        match clone_export_repositories(root, std::slice::from_ref(repository)) {
            Ok(paths) => {
                if let Some(repo_path) = paths.get(&local_id) {
                    project
                        .repos
                        .push(project_repo_entry_from_export(repository, repo_path));
                    added.push(local_id);
                }
            }
            Err(error) => failed.push((local_id, format!("{error:#}"))),
        }
    }

    if added.is_empty() && removed.is_empty() && failed.is_empty() {
        return Ok(());
    }

    if !added.is_empty() || !removed.is_empty() {
        project.repos.sort_by(|a, b| a.id.cmp(&b.id));
        project.updated_at = now_iso();
        write_json(&project_path(root, &project.id), project)?;
    }

    for id in &added {
        println!(
            "{} {} {}",
            out::heading("Project repo:"),
            out::movement("added"),
            out::repo(id)
        );
    }
    for id in &removed {
        println!(
            "{} {} {}",
            out::heading("Project repo:"),
            out::movement("removed"),
            out::repo(id)
        );
    }
    for (id, reason) in &failed {
        println!(
            "{} {}: {}",
            out::warn("Project repo add failed:"),
            out::repo(id),
            out::muted(reason)
        );
    }

    Ok(())
}

/// Pull one named bundle's recorded state from a prepared remote context:
/// localize the remote artifact, fast-forward its feature checkouts, and save.
/// Works for any bundle by id, not just the resolved one, so a workspace-wide
/// pull can process every open bundle. Callers must serialize git work that
/// touches shared source repos; this function only mutates the named bundle's
/// own artifact and checkouts.
///
/// With `merge` set, diverged ledgers are union-merged (`merge_ledgers`)
/// instead of skipped: the saved artifact records both sides' nodes, and any
/// feature checkout that cannot fast-forward onto origin is reported for
/// manual git-level merging without failing the artifact merge.
/// With `materialize` set (the resolved/active bundle), an artifact that is
/// already current still gets its feature branches fetched, missing worktrees
/// created, and checkouts fast-forwarded — `knit fetch` advances the artifact
/// without touching checkouts, so pull must close that gap or a fetched bundle
/// never becomes usable. Without `materialize`, only checkouts that already
/// exist on disk are refreshed; none are created.
pub fn pull_bundle_remote_state(
    root: &Path,
    context: &RemotePullContext,
    bundle_id: &str,
    merge: bool,
    materialize: bool,
) -> Result<RemoteBundleOutcome> {
    let path = bundle_path(root, bundle_id);
    if !path.exists() {
        return Ok(RemoteBundleOutcome::Skipped(
            "no local bundle artifact".to_string(),
        ));
    }
    // Hold the same per-bundle lock mutating commands take, so a pull cannot
    // interleave with a concurrent commit/sync in another knit process.
    let _lock = acquire_named_lock(root, bundle_id)?;
    let local: ChangeGroup = read_json(&path)?;
    let Some(remote_bundle) = context
        .export
        .bundles
        .iter()
        .find(|bundle| bundle.slug == bundle_id)
    else {
        return Ok(RemoteBundleOutcome::Skipped(
            "not present on remote".to_string(),
        ));
    };
    let Some(artifact) = remote_bundle.current_artifact.as_ref() else {
        return Ok(RemoteBundleOutcome::Skipped(
            "no remote artifact".to_string(),
        ));
    };
    let remote_payload = decode_bundle_payload(&artifact.payload, &remote_bundle.slug)?;
    match ledger_relation(&local.node_id_sequence(), &remote_payload.node_id_sequence()) {
        LedgerRelation::Equal => {
            return refresh_bundle_checkouts(root, path, local, materialize, "up to date")
        }
        LedgerRelation::LocalAhead => {
            return refresh_bundle_checkouts(
                root,
                path,
                local,
                materialize,
                "local is ahead of remote",
            )
        }
        LedgerRelation::Diverged if !merge => {
            return Ok(RemoteBundleOutcome::Skipped(format!(
                "bundle {bundle_id}: local and remote ledgers have diverged; run `knit pull --merge` to combine them"
            )))
        }
        LedgerRelation::Diverged => {
            let localized = localize_bundle(remote_payload, &context.project)?;
            prepare_feature_branches(&localized)?;
            let merged = merge_ledgers(&local, &localized, now_iso());
            let mut active = ActiveBundle::unlocked(root.to_path_buf(), path, merged);
            materialize_repos(&mut active, None)?;
            // The artifact merge stands on its own: checkouts that cannot
            // fast-forward have genuinely diverged git branches and need a
            // manual merge in the worktree, after which `knit sync` and the
            // next push reconcile the recorded heads.
            if let Err(error) = fast_forward_feature_checkouts(&mut active) {
                println!(
                    "{} {error:#}",
                    out::warn("feature checkouts did not fast-forward:")
                );
                println!(
                    "{}",
                    out::muted(
                        "Merged ledger saved. Merge origin/<branch> in the affected worktrees, then commit and `knit push`."
                    )
                );
            }
            save_active_bundle(&active)?;
            return Ok(RemoteBundleOutcome::Merged(artifact.artifact_hash.clone()));
        }
        LedgerRelation::RemoteAhead => {}
    }
    let localized = localize_bundle(remote_payload, &context.project)?;
    prepare_feature_branches(&localized)?;
    ensure_remote_bundle_fast_forward(&local, &localized)?;
    let mut active = ActiveBundle::unlocked(root.to_path_buf(), path, localized);
    materialize_repos(&mut active, None)?;
    fast_forward_feature_checkouts(&mut active)?;
    save_active_bundle(&active)?;
    Ok(RemoteBundleOutcome::Pulled(artifact.artifact_hash.clone()))
}

/// Refresh a bundle whose artifact needs no update. Fetches feature branches,
/// materializes missing worktrees when `materialize` is set, re-records
/// checkouts that exist on disk but are missing from the artifact (a
/// remote-localized artifact carries no worktree paths), and fast-forwards
/// every checkout onto origin. Returns `Skipped(reason)` when there was
/// nothing to touch, so callers keep today's quiet no-op behavior.
fn refresh_bundle_checkouts(
    root: &Path,
    path: std::path::PathBuf,
    bundle: ChangeGroup,
    materialize: bool,
    reason: &str,
) -> Result<RemoteBundleOutcome> {
    let mut existing_dirs: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut unrecorded_on_disk = Vec::new();
    let mut absent = Vec::new();
    for repo in &bundle.repos {
        if repo.feature_branch.is_none() {
            continue;
        }
        if let Some(dir) = recorded_checkout_dir(root, repo) {
            existing_dirs.push((repo.id.clone(), dir));
            continue;
        }
        // A worktree can exist at the conventional location even though the
        // artifact does not record it — the state a remote-localized artifact
        // leaves behind for checkouts created earlier. Re-record it.
        let conventional = root.join(".knit/worktrees").join(&bundle.id).join(&repo.id);
        if conventional.exists() {
            existing_dirs.push((repo.id.clone(), conventional));
            unrecorded_on_disk.push(repo.id.clone());
        } else {
            absent.push(repo.id.clone());
        }
    }

    let mut to_materialize = unrecorded_on_disk;
    if materialize {
        to_materialize.extend(absent.iter().cloned());
    }
    if existing_dirs.is_empty() && to_materialize.is_empty() {
        return Ok(RemoteBundleOutcome::Skipped(reason.to_string()));
    }

    let checkout_head = |dir: &Path| crate::git::git_output(dir, ["rev-parse", "HEAD"]).ok();
    let heads_before: Vec<Option<String>> = existing_dirs
        .iter()
        .map(|(_, dir)| checkout_head(dir))
        .collect();
    prepare_feature_branches(&bundle)?;
    let mut active = ActiveBundle::unlocked(root.to_path_buf(), path, bundle);
    let mut created = 0usize;
    if !to_materialize.is_empty() {
        materialize_repos(&mut active, Some(&to_materialize))?;
        if materialize {
            created = absent.len();
        }
        if created > 0 {
            crate::commands::agents::write_bundle_worktree_agents_md(&active)?;
        }
    }
    fast_forward_feature_checkouts(&mut active)?;

    let advanced = existing_dirs
        .iter()
        .zip(heads_before.iter())
        .filter(|((_, dir), before)| checkout_head(dir) != **before)
        .count();
    let rerecorded = to_materialize.len() - created;
    if created == 0 && advanced == 0 && rerecorded == 0 {
        return Ok(RemoteBundleOutcome::Skipped(reason.to_string()));
    }

    save_active_bundle(&active)?;
    let mut parts = Vec::new();
    if created > 0 {
        parts.push(format!("materialized {created} checkout(s)"));
    }
    if advanced > 0 {
        parts.push(format!("fast-forwarded {advanced} checkout(s)"));
    }
    if parts.is_empty() {
        parts.push("re-recorded existing checkout(s)".to_string());
    }
    Ok(RemoteBundleOutcome::Refreshed(parts.join(", ")))
}

/// The checkout dir (worktree path or in-place) the artifact records, when it
/// exists on disk.
fn recorded_checkout_dir(
    root: &Path,
    repo: &crate::model::RepoEntry,
) -> Option<std::path::PathBuf> {
    if let Some(worktree_path) = &repo.worktree_path {
        let path = std::path::PathBuf::from(worktree_path);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        return path.exists().then_some(path);
    }
    if crate::checkout::is_in_place(repo) {
        let path = std::path::PathBuf::from(&repo.path);
        return path.exists().then_some(path);
    }
    None
}

pub fn pull_remote_state(remote_name: Option<&str>, skip_remote: bool, merge: bool) -> Result<()> {
    let Some(context) = prepare_remote_pull(remote_name, skip_remote)? else {
        return Ok(());
    };
    let active = load_active_bundle()?;
    match pull_bundle_remote_state(&active.root, &context, &active.bundle.id, merge, true)? {
        RemoteBundleOutcome::Pulled(hash) => println!(
            "{} {} {}",
            out::movement("pulled"),
            out::repo(&active.bundle.id),
            out::muted(&hash)
        ),
        RemoteBundleOutcome::Merged(hash) => println!(
            "{} {} {}",
            out::movement("merged ledgers"),
            out::repo(&active.bundle.id),
            out::muted(&hash)
        ),
        RemoteBundleOutcome::Refreshed(summary) => println!(
            "{} {} {}",
            out::movement("refreshed"),
            out::repo(&active.bundle.id),
            out::muted(&summary)
        ),
        RemoteBundleOutcome::Skipped(reason) => println!(
            "{} {}",
            out::warn("KnitHub pull skipped:"),
            out::muted(reason)
        ),
    }
    Ok(())
}

/// Look up a bundle slug on the primary sync remote, returning the remote
/// record's lifecycle state when a non-deleted bundle with that slug already
/// exists. Used at bundle creation to catch two users independently picking
/// the same title for different features. Callers treat any error (no remote,
/// no token, offline) as "unknown" so creation keeps working offline.
pub fn remote_bundle_lifecycle(
    config: &KnitConfig,
    project_id: &str,
    bundle_id: &str,
) -> Result<Option<String>> {
    let Some(remote_name) = configured_sync_remote_names(config).into_iter().next() else {
        return Ok(None);
    };
    let remote = resolve_remote(config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;
    let export = fetch_project_export(remote, &token, project_id)?;
    Ok(export
        .bundles
        .into_iter()
        .find(|bundle| bundle.slug == bundle_id && bundle.lifecycle_state != "deleted")
        .map(|bundle| bundle.lifecycle_state))
}

/// List the bundle records the sync remote holds for `project_id`, decoding each
/// bundle's current artifact payload when it is a supported Knit bundle.
pub fn list_remote_bundles(
    config: &KnitConfig,
    project_id: &str,
) -> Result<Vec<RemoteBundleRecord>> {
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
/// Archive a KnitHub bundle record in place. Used by prune's remote-orphan
/// cleanup: a record whose local artifact is gone is finished history, not
/// noise, so the remote keeps it (hidden from active views) instead of
/// tombstoning it. Rides the everyday `bundle:push` scope.
pub fn archive_remote_bundle_by_id(config: &KnitConfig, remote_id: &str) -> Result<String> {
    let remote_name = resolve_sync_remote_name(config)?;
    let remote = resolve_remote(config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;
    let archived: RemoteBundle = request_json(
        remote,
        &token,
        "PATCH",
        &format!("/bundles/{remote_id}/archive"),
        None,
    )?;
    Ok(archived.slug)
}

pub fn delete_remote_bundle_by_id(config: &KnitConfig, remote_id: &str) -> Result<String> {
    let remote_name = resolve_sync_remote_name(config)?;
    let remote = resolve_remote(config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;
    let deleted: RemoteBundle = request_json(
        remote,
        &token,
        "DELETE",
        &format!("/bundles/{remote_id}"),
        None,
    )?;
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
    let remote_name = remote_name
        .map(crate::ids::slugify)
        .or_else(|| configured_sync_remote_names(config).into_iter().next())
        .with_context(|| {
            "No remote configured for bundle fetch. Configure a remote with `knit remote add` or set sync-remotes."
        })?;
    let remote = resolve_remote(config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;

    let project_id = config
        .active_project
        .clone()
        .context("Bundle fetch requires active_project. Set with `knit init <name>`.")?;

    let export = fetch_project_export(remote, &token, &project_id)?;
    crate::history::append_history_events(root, &project_id, &export.history_events)?;

    let Some(local_project) = load_project_if_present(root, &project_id)? else {
        bail!("No local project `{project_id}` found. Cannot localize bundles.");
    };

    let bundles_dir = root.join(".knit/bundles");
    fs::create_dir_all(&bundles_dir).with_context(|| {
        format!(
            "failed to create bundles directory {}",
            bundles_dir.display()
        )
    })?;

    let mut fetched_count = 0;
    let mut quarantined_count = 0;
    for remote_bundle in export.bundles {
        if remote_bundle.lifecycle_state == "deleted" {
            continue;
        }
        let Some(artifact) = remote_bundle.current_artifact.as_ref() else {
            continue;
        };

        let mut bundle = decode_bundle_payload(&artifact.payload, &remote_bundle.slug)
            .with_context(|| format!("failed to decode bundle `{}`", remote_bundle.slug))?;
        let branch_mapping = bundle_branch_mapping(&bundle);

        let bundle_path = bundles_dir.join(format!("{}.bundle.json", bundle.id));
        // Discovery is for bundles you might act on: a remote bundle with no
        // local artifact is only localized while it is open. Resurrecting the
        // project's full landed/archived history would flood the workspace
        // and undo `knit bundle prune` on every sync. Existing local
        // artifacts still fast-forward whatever their state, so work landed
        // or archived on another machine is reflected here.
        let status;
        if !bundle_path.exists() {
            if remote_bundle.lifecycle_state != "open" {
                continue;
            }
            // The remote lifecycle can still read "open" for a bundle that was
            // landed or pruned here (nothing pushes terminal state back), so
            // the local delete quarantine is the authority: a bundle deleted
            // locally stays deleted.
            if root
                .join(".knit/deleted/bundles")
                .join(format!("{}.bundle.json", bundle.id))
                .exists()
            {
                quarantined_count += 1;
                continue;
            }
            bundle = localize_bundle(bundle, &local_project)
                .with_context(|| format!("failed to localize bundle `{}`", remote_bundle.slug))?;
            crate::store::write_json(&bundle_path, &bundle)
                .with_context(|| format!("failed to write bundle `{}`", remote_bundle.slug))?;
            fetched_count += 1;
            status = out::movement("new").to_string();
        } else {
            // An existing local artifact is only refreshed when the remote
            // ledger is strictly ahead (a fast-forward). Equal/local-ahead
            // artifacts are left untouched; diverged ledgers keep local.
            let local: ChangeGroup = read_json(&bundle_path)
                .with_context(|| format!("failed to read local bundle `{}`", bundle.id))?;
            match ledger_relation(&local.node_id_sequence(), &bundle.node_id_sequence()) {
                LedgerRelation::Equal => status = out::muted("up to date").to_string(),
                LedgerRelation::LocalAhead => status = out::muted("local ahead").to_string(),
                LedgerRelation::Diverged => {
                    status = out::warn(
                        "diverged; kept local (run `knit pull --merge` to combine the ledgers)",
                    )
                    .to_string()
                }
                LedgerRelation::RemoteAhead => {
                    bundle = localize_bundle(bundle, &local_project).with_context(|| {
                        format!("failed to localize bundle `{}`", remote_bundle.slug)
                    })?;
                    // Localizing wipes checkout recordings (they are per
                    // machine); carry over this workspace's so an artifact
                    // fast-forward does not orphan existing worktrees.
                    for repo in &mut bundle.repos {
                        if repo.worktree_path.is_none() {
                            repo.worktree_path = local
                                .repos
                                .iter()
                                .find(|local_repo| local_repo.id == repo.id)
                                .and_then(|local_repo| local_repo.worktree_path.clone());
                        }
                    }
                    crate::store::write_json(&bundle_path, &bundle).with_context(|| {
                        format!("failed to write bundle `{}`", remote_bundle.slug)
                    })?;
                    fetched_count += 1;
                    status = out::movement("updated").to_string();
                }
            }
        }
        println!(
            "  {} {} {} [{status}]",
            out::node(&remote_bundle.slug),
            out::muted(&remote_bundle.lifecycle_state),
            branch_mapping
        );
    }

    if quarantined_count > 0 {
        println!(
            "  {}",
            out::muted(format!(
                "{quarantined_count} locally deleted bundle(s) left deleted"
            ))
        );
    }
    if fetched_count > 0 {
        println!(
            "{} {} bundle(s) from {}",
            out::movement("fetched"),
            out::ok(fetched_count),
            out::repo(&remote_name)
        );
    } else {
        println!(
            "{} no bundles to fetch from {}",
            out::muted("already up-to-date"),
            out::repo(&remote_name)
        );
    }
    Ok(())
}

/// Render a bundle's repo -> feature-branch mapping for fetch/list output, so
/// discovery answers "which branches does this bundle map to" without opening
/// the artifact.
fn bundle_branch_mapping(bundle: &ChangeGroup) -> String {
    bundle
        .repos
        .iter()
        .map(|repo| {
            let branch = repo
                .feature_branch
                .clone()
                .unwrap_or_else(|| format!("knit/{}", bundle.id));
            format!("{} -> {}", out::repo(&repo.id), out::branch(&branch))
        })
        .collect::<Vec<_>>()
        .join(", ")
}
