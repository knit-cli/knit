//! Pull recorded bundle state from a sync remote (one bundle or workspace
//! wide), list/fetch remote bundles, and delete remote bundle records.

use super::client::{
    configured_sync_remote_names, decode_bundle_payload, effective_workspace_config,
    ensure_remote_bundle_fast_forward, fast_forward_feature_checkouts, fetch_bundle_artifact,
    fetch_project_export, load_project_if_present, localize_bundle, prepare_feature_branches,
    request_json, resolve_export_bundle_payload, resolve_project_id, resolve_remote,
    resolve_sync_remote_name, resolve_token, with_first_available_remote,
};
use super::clone::{
    clone_export_repositories, export_repo_local_id, materialize_imported_bundle,
    project_repo_entry_from_export,
};
use super::{
    print_json_error_envelope, RemoteBundle, RemoteErrorKind, RemoteExportBundle,
    RemoteExportRepository, RemoteProjectExport, RemoteViews,
};
use crate::commands::worktree::materialize_repos;
use crate::ids::slugify;
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
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

/// Pull the current user's saved views for a project from the sync remote,
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
/// The export is slim (no artifact payloads), so the context also carries the
/// remote it came from: payloads are fetched one bundle at a time, on demand.
pub struct RemotePullContext {
    project: KnitProject,
    export: RemoteProjectExport,
    remote_name: String,
    remote: KnitRemote,
    token: Option<String>,
    /// Serializes per-bundle artifact fetches. `knit pull` walks the open
    /// bundles on parallel threads, and bounded server memory — one payload
    /// built at a time — is the entire point of the incremental fetch.
    fetch_gate: Mutex<()>,
}

impl RemotePullContext {
    /// The remote bundle's payload plus its artifact hash: the inlined copy
    /// when the server sent one, otherwise a single sequential fetch.
    fn bundle_payload(&self, entry: &RemoteExportBundle) -> Result<(ChangeGroup, String)> {
        let _gate = self
            .fetch_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        resolve_export_bundle_payload(&self.remote, self.token.as_deref(), entry)
    }
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

/// A bundle as it exists on the configured sync remote. `payload` is filled
/// only when the export inlined one (an older server); with the slim export it
/// stays `None` and callers that need the payload fetch it per bundle with
/// [`fetch_remote_bundle_payload`], for the records they actually care about.
pub struct RemoteBundleRecord {
    pub remote_id: String,
    pub slug: String,
    pub lifecycle_state: String,
    pub payload: Option<ChangeGroup>,
}

/// Resolve a sync remote and fetch the project export a single time. Returns
/// `None` when the pull opts out (`--no-remote`), no remote is configured, or
/// no configured remote is reachable, so callers can skip the artifact step
/// without it being an error — the git side of a pull never depends on a
/// remote answering. Implicit resolution walks the configured sync remotes in
/// order and uses the first one that responds; each unreachable remote is
/// reported and skipped. An explicit `--remote` override still fails hard,
/// because the caller named exactly the remote they wanted.
pub fn prepare_remote_pull(
    remote_override: Option<&str>,
    skip_remote: bool,
) -> Result<Option<RemotePullContext>> {
    if skip_remote {
        return Ok(None);
    }
    let (root, config) = effective_workspace_config()?;
    let candidates: Vec<String> = match remote_override {
        Some(name) => vec![crate::ids::slugify(name)],
        None => configured_sync_remote_names(&config),
    };
    if candidates.is_empty() {
        return Ok(None);
    }
    let explicit = remote_override.is_some();
    let project_id = config
        .active_project
        .clone()
        .context("No active project selected for remote pull. Run `knit init <name>`.")?;
    let mut project = load_project_if_present(&root, &project_id)?
        .with_context(|| format!("No local Knit project named `{project_id}`."))?;
    for remote_name in candidates {
        let attempt = resolve_remote(&config, &remote_name)
            .and_then(|remote| Ok((remote.clone(), resolve_token(&remote_name, remote)?)))
            .and_then(|(remote, token)| {
                let export = fetch_project_export(&remote, Some(&token), &project_id)?;
                Ok((remote, token, export))
            });
        match attempt {
            Ok((remote, token, export)) => {
                crate::history::append_history_events(
                    &root,
                    &project_id,
                    &export.decoded_history_events(&project_id),
                )?;
                reconcile_project_repositories(&root, &mut project, &export)?;
                return Ok(Some(RemotePullContext {
                    project,
                    export,
                    remote_name,
                    remote,
                    token: Some(token),
                    fetch_gate: Mutex::new(()),
                }));
            }
            Err(error) => {
                if explicit {
                    return Err(error);
                }
                println!(
                    "{} {error:#}",
                    out::warn(format!("remote {remote_name} unavailable, skipping:"))
                );
            }
        }
    }
    println!(
        "{}",
        out::warn("No sync remote reachable; continuing without remote sync.")
    );
    Ok(None)
}

/// Reconcile the local project's tracked repositories with the remote export so
/// that repositories added or removed on the remote flow into an existing
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
    // The export only carries artifact metadata. When its hash is the one this
    // artifact was last reconciled with, the remote holds nothing new and the
    // payload is never downloaded — only the checkouts are refreshed.
    if artifact.payload.is_none()
        && local.synced_artifact_hash(&context.remote_name) == Some(artifact.artifact_hash.as_str())
    {
        return refresh_bundle_checkouts(root, path, local, materialize, "up to date");
    }
    let (remote_payload, artifact_hash) = context.bundle_payload(remote_bundle)?;
    match ledger_relation(&local.node_id_sequence(), &remote_payload.node_id_sequence()) {
        LedgerRelation::Equal => {
            let local =
                record_synced_artifact(&path, local, context, remote_bundle, &artifact_hash)?;
            return refresh_bundle_checkouts(root, path, local, materialize, "up to date");
        }
        LedgerRelation::LocalAhead => {
            let local =
                record_synced_artifact(&path, local, context, remote_bundle, &artifact_hash)?;
            return refresh_bundle_checkouts(
                root,
                path,
                local,
                materialize,
                "local is ahead of remote",
            );
        }
        LedgerRelation::Diverged if !merge => {
            return Ok(RemoteBundleOutcome::Skipped(format!(
                "bundle {bundle_id}: local and remote ledgers have diverged; run `knit pull --merge` to combine them"
            )))
        }
        LedgerRelation::Diverged => {
            let localized = localize_bundle(remote_payload, &context.project)?;
            prepare_feature_branches(&localized)?;
            let mut merged = merge_ledgers(&local, &localized, now_iso());
            merged.record_sync_target_with_artifact(
                &context.remote_name,
                &remote_bundle.id,
                &context.remote.url,
                Some(&artifact_hash),
            );
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
            return Ok(RemoteBundleOutcome::Merged(artifact_hash));
        }
        LedgerRelation::RemoteAhead => {}
    }
    let mut localized = localize_bundle(remote_payload, &context.project)?;
    localized.record_sync_target_with_artifact(
        &context.remote_name,
        &remote_bundle.id,
        &context.remote.url,
        Some(&artifact_hash),
    );
    prepare_feature_branches(&localized)?;
    ensure_remote_bundle_fast_forward(&local, &localized)?;
    let mut active = ActiveBundle::unlocked(root.to_path_buf(), path, localized);
    materialize_repos(&mut active, None)?;
    fast_forward_feature_checkouts(&mut active)?;
    save_active_bundle(&active)?;
    Ok(RemoteBundleOutcome::Pulled(artifact_hash))
}

/// Record on the local artifact which remote artifact it is in sync with, so
/// the next pull can decide from the slim export alone that there is nothing
/// to download. Persists only when the recording changed something; the
/// bundle's own `updatedAt` is deliberately left alone, since nothing about
/// the recorded work changed.
fn record_synced_artifact(
    path: &Path,
    mut bundle: ChangeGroup,
    context: &RemotePullContext,
    remote_bundle: &RemoteExportBundle,
    artifact_hash: &str,
) -> Result<ChangeGroup> {
    if bundle.record_sync_target_with_artifact(
        &context.remote_name,
        &remote_bundle.id,
        &context.remote.url,
        Some(artifact_hash),
    ) {
        write_json(path, &bundle)?;
    }
    Ok(bundle)
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
            out::warn("remote pull skipped:"),
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
    let mut last_error: Option<anyhow::Error> = None;
    for remote_name in configured_sync_remote_names(config) {
        let attempt = resolve_remote(config, &remote_name)
            .and_then(|remote| Ok((remote, resolve_token(&remote_name, remote)?)))
            .and_then(|(remote, token)| fetch_project_export(remote, Some(&token), project_id));
        match attempt {
            Ok(export) => {
                return Ok(export
                    .bundles
                    .into_iter()
                    .find(|bundle| bundle.slug == bundle_id && bundle.lifecycle_state != "deleted")
                    .map(|bundle| bundle.lifecycle_state));
            }
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

/// List the bundle records the sync remote holds for `project_id`. Payloads are
/// carried over only when the server inlined them; the slim export leaves them
/// out, and callers fetch the few they need one at a time.
pub fn list_remote_bundles(
    config: &KnitConfig,
    project_id: &str,
) -> Result<Vec<RemoteBundleRecord>> {
    let export = with_first_available_remote(config, None, |_, remote, token| {
        fetch_project_export(remote, Some(token), project_id)
    })?;
    Ok(export
        .bundles
        .into_iter()
        .map(|bundle| {
            let payload = bundle.current_artifact.as_ref().and_then(|artifact| {
                artifact
                    .payload
                    .as_ref()
                    .and_then(|payload| decode_bundle_payload(payload, &bundle.slug).ok())
            });
            RemoteBundleRecord {
                remote_id: bundle.id,
                slug: bundle.slug,
                lifecycle_state: bundle.lifecycle_state,
                payload,
            }
        })
        .collect())
}

/// Fetch one listed remote bundle's artifact payload by its remote id. Callers
/// walk their candidates sequentially: the server builds one payload at a time.
pub fn fetch_remote_bundle_payload(
    config: &KnitConfig,
    remote_id: &str,
    slug: &str,
) -> Result<ChangeGroup> {
    with_first_available_remote(config, None, |_, remote, token| {
        fetch_bundle_artifact(remote, Some(token), remote_id, slug)
    })
    .map(|(payload, _)| payload)
}

/// Delete a single bundle record from the sync remote by its remote id, returning the
/// deleted bundle's slug.
/// Archive a remote bundle record in place. Used by prune's remote-orphan
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
    let export = fetch_project_export(remote, Some(&token), &project_id)?;
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

/// Archive a bundle's record on every configured sync remote after its local
/// artifact was deleted, so hosted dashboards stop counting deleted work as
/// open. Archive, never delete: the record stays as durable history, and true
/// remote deletion remains the explicit `--remote-bundles` door. Records
/// already archived or tombstoned on a remote are left alone, and a bundle
/// with no resolvable project is a silent no-op.
pub fn archive_deleted_bundle_on_remotes(config: &KnitConfig, bundle: &ChangeGroup) -> Result<()> {
    let Some(project_id) = bundle
        .project_id
        .clone()
        .or_else(|| config.active_project.clone())
    else {
        return Ok(());
    };
    let mut failures = Vec::new();
    for remote_name in configured_sync_remote_names(config) {
        match archive_bundle_record_on_remote(config, &remote_name, &project_id, &bundle.id) {
            Ok(Some(slug)) => println!(
                "{}: {} {}",
                out::node(&bundle.id),
                out::movement("archived remote bundle"),
                out::muted(format!("{remote_name}/{slug}"))
            ),
            Ok(None) => {}
            Err(error) => failures.push(format!("{remote_name}: {error:#}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "failed to archive the remote bundle record on {} remote(s):\n{}",
            failures.len(),
            failures.join("\n")
        )
    }
}

fn archive_bundle_record_on_remote(
    config: &KnitConfig,
    remote_name: &str,
    project_id: &str,
    bundle_id: &str,
) -> Result<Option<String>> {
    let remote = resolve_remote(config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let export = fetch_project_export(remote, Some(&token), project_id)?;
    let Some(record) = export.bundles.iter().find(|record| {
        record.slug == bundle_id
            && record.lifecycle_state != "deleted"
            && record.lifecycle_state != "archived"
    }) else {
        return Ok(None);
    };
    let archived: RemoteBundle = request_json(
        remote,
        &token,
        "PATCH",
        &format!("/bundles/{}/archive", record.id),
        None,
    )?;
    Ok(Some(archived.slug))
}

pub fn fetch_bundles_from_remote(
    root: &Path,
    config: &KnitConfig,
    remote_name: Option<&str>,
) -> Result<()> {
    let project_id = config
        .active_project
        .clone()
        .context("Bundle fetch requires active_project. Set with `knit init <name>`.")?;

    let (remote_name, remote, token, export) =
        with_first_available_remote(config, remote_name, |name, remote, token| {
            Ok((
                name.to_string(),
                remote.clone(),
                token.to_string(),
                fetch_project_export(remote, Some(token), &project_id)?,
            ))
        })?;
    crate::history::append_history_events(
        root,
        &project_id,
        &export.decoded_history_events(&project_id),
    )?;

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

        // A remote bundle's local artifact is the one its slug names (a
        // bundle's slug is its local id). Everything that can be decided from
        // the slim export alone is decided here, before any payload is
        // downloaded: the sweep must not pull an artifact it has no use for.
        let bundle_path = bundles_dir.join(format!("{}.bundle.json", remote_bundle.slug));
        let local: Option<ChangeGroup> =
            if bundle_path.exists() {
                Some(read_json(&bundle_path).with_context(|| {
                    format!("failed to read local bundle `{}`", remote_bundle.slug)
                })?)
            } else {
                None
            };
        // Discovery is for bundles you might act on: a remote bundle with no
        // local artifact is only localized while it is open. Resurrecting the
        // project's full landed/archived history would flood the workspace
        // and undo `knit bundle prune` on every sync. Existing local
        // artifacts still fast-forward whatever their state, so work landed
        // or archived on another machine is reflected here.
        match local.as_ref() {
            None => {
                if remote_bundle.lifecycle_state != "open" {
                    continue;
                }
                // The remote lifecycle can still read "open" for a bundle that
                // was deleted here (the delete-time remote archive is
                // best-effort and can be skipped offline), so the local delete
                // quarantine is the authority: a bundle deleted locally stays
                // deleted.
                if root
                    .join(".knit/deleted/bundles")
                    .join(format!("{}.bundle.json", remote_bundle.slug))
                    .exists()
                {
                    quarantined_count += 1;
                    continue;
                }
            }
            Some(local) => {
                // The recorded hash still matches the remote's: nothing new to
                // download for this bundle.
                if artifact.payload.is_none()
                    && local.synced_artifact_hash(&remote_name)
                        == Some(artifact.artifact_hash.as_str())
                {
                    println!(
                        "  {} {} {} [{}]",
                        out::node(&remote_bundle.slug),
                        out::muted(&remote_bundle.lifecycle_state),
                        bundle_branch_mapping(local),
                        out::muted("up to date")
                    );
                    continue;
                }
            }
        }

        let (mut bundle, artifact_hash) =
            resolve_export_bundle_payload(&remote, Some(&token), &remote_bundle)
                .with_context(|| format!("failed to fetch bundle `{}`", remote_bundle.slug))?;
        let branch_mapping = bundle_branch_mapping(&bundle);

        let status;
        if let Some(mut local) = local {
            // An existing local artifact is only refreshed when the remote
            // ledger is strictly ahead (a fast-forward). Equal/local-ahead
            // artifacts are left untouched; diverged ledgers keep local.
            //
            // Record which remote artifact the local one is in sync with, so
            // the next sweep can skip this download entirely. A diverged
            // ledger is deliberately not recorded: it must keep reporting
            // divergence until the user combines the two.
            let record_synced = |local: &mut ChangeGroup| -> Result<()> {
                if local.record_sync_target_with_artifact(
                    &remote_name,
                    &remote_bundle.id,
                    &remote.url,
                    Some(&artifact_hash),
                ) {
                    crate::store::write_json(&bundle_path, local).with_context(|| {
                        format!("failed to write bundle `{}`", remote_bundle.slug)
                    })?;
                }
                Ok(())
            };
            match ledger_relation(&local.node_id_sequence(), &bundle.node_id_sequence()) {
                LedgerRelation::Equal => {
                    record_synced(&mut local)?;
                    status = out::muted("up to date").to_string();
                }
                LedgerRelation::LocalAhead => {
                    record_synced(&mut local)?;
                    status = out::muted("local ahead").to_string();
                }
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
                    bundle.record_sync_target_with_artifact(
                        &remote_name,
                        &remote_bundle.id,
                        &remote.url,
                        Some(&artifact_hash),
                    );
                    crate::store::write_json(&bundle_path, &bundle).with_context(|| {
                        format!("failed to write bundle `{}`", remote_bundle.slug)
                    })?;
                    fetched_count += 1;
                    status = out::movement("updated").to_string();
                }
            }
        } else {
            bundle = localize_bundle(bundle, &local_project)
                .with_context(|| format!("failed to localize bundle `{}`", remote_bundle.slug))?;
            bundle.record_sync_target_with_artifact(
                &remote_name,
                &remote_bundle.id,
                &remote.url,
                Some(&artifact_hash),
            );
            crate::store::write_json(&bundle_path, &bundle)
                .with_context(|| format!("failed to write bundle `{}`", remote_bundle.slug))?;
            fetched_count += 1;
            status = out::movement("new").to_string();
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

/// Machine-readable `knit bundle pull --json` result document. The shape is a
/// contract with external drivers (ivaldi); change it only deliberately.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BundlePullDocument {
    bundle: String,
    repos: Vec<BundlePullRepo>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BundlePullRepo {
    id: String,
    feature_branch: Option<String>,
    head_sha: Option<String>,
    status: &'static str,
    worktree_path: Option<String>,
}

/// `knit bundle pull <slug>`: one verb that refreshes a single bundle's
/// artifact from the sync remote, fetches its feature branches (the installed
/// credential helpers apply), and materializes fast-forwarded worktrees.
pub fn pull_bundle_by_slug(slug: &str, json: bool) -> Result<()> {
    if json {
        crate::output::route_human_lines_to_stderr();
    }
    match pull_bundle_by_slug_classified(slug) {
        Ok(document) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&document)
                        .context("failed to serialize bundle pull document")?
                );
            }
            Ok(())
        }
        Err((kind, error)) => {
            if json {
                print_json_error_envelope(kind, &error);
            }
            Err(error)
        }
    }
}

fn pull_bundle_by_slug_classified(
    slug: &str,
) -> std::result::Result<BundlePullDocument, (RemoteErrorKind, anyhow::Error)> {
    let other = |error: anyhow::Error| (RemoteErrorKind::Other, error);
    let slug = slugify(slug);

    let (root, config) = effective_workspace_config().map_err(other)?;
    let project_id = config
        .active_project
        .clone()
        .context("Bundle pull requires an active project. Run `knit init <name>` or clone one.")
        .map_err(other)?;
    let (remote_name, remote, token, export) =
        with_first_available_remote(&config, None, |name, remote, token| {
            Ok((
                name.to_string(),
                remote.clone(),
                token.to_string(),
                fetch_project_export(remote, Some(token), &project_id)?,
            ))
        })
        .map_err(|error| (RemoteErrorKind::Http, error))?;
    crate::history::append_history_events(
        &root,
        &project_id,
        &export.decoded_history_events(&project_id),
    )
    .map_err(other)?;
    let local_project = load_project_if_present(&root, &project_id)
        .map_err(other)?
        .with_context(|| format!("No local Knit project named `{project_id}`."))
        .map_err(other)?;

    let Some(remote_bundle) = export
        .bundles
        .iter()
        .find(|bundle| bundle.slug == slug && bundle.lifecycle_state != "deleted")
    else {
        return Err((
            RemoteErrorKind::NotFound,
            anyhow!("Remote has no bundle named `{slug}`."),
        ));
    };
    if remote_bundle.current_artifact.is_none() {
        return Err((
            RemoteErrorKind::NotFound,
            anyhow!("Remote bundle `{slug}` has no artifact to pull."),
        ));
    }
    // One named bundle, one artifact fetch (or the payload an older server
    // already inlined in the export).
    let (mut bundle, artifact_hash) =
        resolve_export_bundle_payload(&remote, Some(&token), remote_bundle)
            .map_err(|error| (RemoteErrorKind::Http, error))?;
    let bundle_id = bundle.id.clone();
    if root
        .join(".knit/deleted/bundles")
        .join(format!("{bundle_id}.bundle.json"))
        .exists()
    {
        return Err((
            RemoteErrorKind::Other,
            anyhow!("Bundle `{bundle_id}` was deleted locally; restore it before pulling."),
        ));
    }

    let path = bundle_path(&root, &bundle_id);
    {
        let _lock = acquire_named_lock(&root, &bundle_id).map_err(other)?;
        if !path.exists() {
            bundle = localize_bundle(bundle, &local_project).map_err(other)?;
            bundle.record_sync_target_with_artifact(
                &remote_name,
                &remote_bundle.id,
                &remote.url,
                Some(&artifact_hash),
            );
            write_json(&path, &bundle).map_err(other)?;
            crate::human!(
                "{} {} {}",
                out::node(&bundle_id),
                out::movement("fetched"),
                out::muted("bundle artifact")
            );
        } else {
            let local: ChangeGroup = read_json(&path).map_err(other)?;
            match ledger_relation(&local.node_id_sequence(), &bundle.node_id_sequence()) {
                LedgerRelation::Equal | LedgerRelation::LocalAhead => {
                    crate::human!(
                        "{} {}",
                        out::node(&bundle_id),
                        out::muted("artifact already current")
                    );
                }
                LedgerRelation::Diverged => {
                    crate::human!(
                        "{} {}",
                        out::node(&bundle_id),
                        out::warn(
                            "local and remote ledgers have diverged; kept local (run `knit pull --merge` to combine them)"
                        )
                    );
                }
                LedgerRelation::RemoteAhead => {
                    let mut localized = localize_bundle(bundle, &local_project).map_err(other)?;
                    // Localizing wipes per-machine checkout recordings; carry
                    // this workspace's over so the refresh does not orphan
                    // existing worktrees.
                    for repo in &mut localized.repos {
                        if repo.worktree_path.is_none() {
                            repo.worktree_path = local
                                .repos
                                .iter()
                                .find(|local_repo| local_repo.id == repo.id)
                                .and_then(|local_repo| local_repo.worktree_path.clone());
                        }
                    }
                    localized.record_sync_target_with_artifact(
                        &remote_name,
                        &remote_bundle.id,
                        &remote.url,
                        Some(&artifact_hash),
                    );
                    write_json(&path, &localized).map_err(other)?;
                    crate::human!(
                        "{} {} {}",
                        out::node(&bundle_id),
                        out::movement("advanced"),
                        out::muted("bundle artifact")
                    );
                }
            }
        }
    }

    // Fetch feature branches, create missing worktrees, fast-forward to the
    // recorded heads — the same machinery a clone-time materialize uses. The
    // installed exact-host credential helpers carry non-public repo access.
    super::helpers::ensure_helpers_for_git(&remote_name);
    materialize_imported_bundle(&root, &bundle_id).map_err(other)?;

    let saved: ChangeGroup = read_json(&path).map_err(other)?;
    let repos = saved
        .repos
        .iter()
        .map(|repo| BundlePullRepo {
            id: repo.id.clone(),
            feature_branch: repo.feature_branch.clone(),
            head_sha: repo.head_sha.clone(),
            status: "pulled",
            worktree_path: repo.worktree_path.as_ref().map(|worktree_path| {
                let candidate = std::path::PathBuf::from(worktree_path);
                if candidate.is_absolute() {
                    candidate.display().to_string()
                } else {
                    root.join(candidate).display().to_string()
                }
            }),
        })
        .collect::<Vec<_>>();

    crate::human!(
        "{} {} {} repo(s) ready",
        out::movement("pulled"),
        out::node(&bundle_id),
        repos.len()
    );

    Ok(BundlePullDocument {
        bundle: bundle_id,
        repos,
    })
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
