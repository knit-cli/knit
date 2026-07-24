//! `knit clone` — import a remote project export into a fresh local workspace:
//! clone its repositories, write the project and bundle artifacts, and
//! optionally materialize the active bundle.

use super::client::{
    configured_sync_remote_names, decode_bundle_payload, fast_forward_feature_checkouts,
    fetch_project_export, localize_bundle, normalize_base_url, prepare_feature_branches,
    token_from_env,
};
use super::credentials::NO_ACCESS_HINT;
use super::{
    print_json_error_envelope, RemoteErrorKind, RemoteExportRepository, RemoteProjectExport,
};
use crate::commands::agents::{
    print_bundle_worktree_agents_summary, write_bundle_worktree_agents_md,
};
use crate::commands::worktree::materialize_repos;
use crate::git::{current_branch, git_output, is_git_worktree, ref_exists};
use crate::ids::slugify;
use crate::model::{
    ChangeGroup, CheckoutMode, KnitConfig, KnitProject, KnitRemote, ProjectRepoEntry,
    SCHEMA_VERSION,
};
use crate::output as out;
use crate::store::{
    bundle_path, find_knit_root, project_path, read_json, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Machine-readable `knit clone --json` result document. The shape is a
/// contract with external drivers (ivaldi); change it only deliberately.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CloneDocument {
    project: CloneDocumentProject,
    target_path: String,
    repos: Vec<CloneDocumentRepo>,
    cloned_repo_count: usize,
    failed_repo_count: usize,
    omitted_repository_count: u64,
    bundles: CloneDocumentBundles,
    active_bundle: Option<String>,
    worktrees_materialized: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloneDocumentProject {
    id: String,
    /// Username or org slug half of the `owner/slug` clone reference. Null when
    /// the clone used a bare slug and the export carried no organization.
    owner: Option<String>,
    slug: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloneDocumentRepo {
    id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloneDocumentBundles {
    restored: Vec<String>,
    dropped: Vec<DroppedBundle>,
}

/// A bundle the export carried but the clone could not restore because one or
/// more of its repos were not cloned (failed or withheld by the server).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DroppedBundle {
    pub(super) id: String,
    pub(super) missing_repos: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn clone_project_from_remote(
    project_identifier: &str,
    target: Option<&Path>,
    remote_name: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
    active_bundle: Option<&str>,
    materialize: bool,
    json: bool,
) -> Result<()> {
    if json {
        crate::output::route_human_lines_to_stderr();
    }
    match clone_project_classified(
        project_identifier,
        target,
        remote_name,
        url,
        token,
        active_bundle,
        materialize,
    ) {
        Ok(document) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&document)
                        .context("failed to serialize clone result document")?
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

/// Run the clone, tagging every failure with its machine-readable error kind so
/// the `--json` wrapper can emit the contract error envelope.
fn clone_project_classified(
    project_identifier: &str,
    target: Option<&Path>,
    remote_name: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
    active_bundle: Option<&str>,
    materialize: bool,
) -> std::result::Result<CloneDocument, (RemoteErrorKind, anyhow::Error)> {
    let (remote_name, remote, stored_token, token) =
        resolve_remote_for_clone_classified(remote_name, url, token)?;
    let export = fetch_project_export(&remote, &token, project_identifier)
        .map_err(|error| (RemoteErrorKind::Http, error))?;
    clone_fetched_export(
        project_identifier,
        target,
        remote_name,
        remote,
        stored_token,
        token,
        export,
        active_bundle,
        materialize,
    )
    .map_err(|error| (RemoteErrorKind::Other, error))
}

#[allow(clippy::too_many_arguments)]
fn clone_fetched_export(
    project_identifier: &str,
    target: Option<&Path>,
    remote_name: String,
    remote: KnitRemote,
    stored_token: Option<String>,
    token: String,
    export: RemoteProjectExport,
    active_bundle: Option<&str>,
    materialize: bool,
) -> Result<CloneDocument> {
    let target_root = resolve_clone_target(target, project_identifier)?;
    prepare_clone_target(&target_root)?;

    fs::create_dir_all(target_root.join(".knit/projects")).with_context(|| {
        format!(
            "failed to create {}",
            target_root.join(".knit/projects").display()
        )
    })?;
    fs::create_dir_all(target_root.join(".knit/bundles")).with_context(|| {
        format!(
            "failed to create {}",
            target_root.join(".knit/bundles").display()
        )
    })?;
    fs::create_dir_all(target_root.join(".knit/worktrees")).with_context(|| {
        format!(
            "failed to create {}",
            target_root.join(".knit/worktrees").display()
        )
    })?;

    super::helpers::ensure_helpers_for_git(&remote_name);
    let (repo_paths, failed_repos) =
        clone_export_repositories_collecting(&target_root, &export.repositories);
    if repo_paths.is_empty() {
        bail!(
            "Failed to clone any repository for project `{}`:\n{}",
            export.project.slug,
            format_repo_failures(&failed_repos)
        );
    }
    let project = local_project_from_export(&export, &repo_paths)?;
    write_json(&project_path(&target_root, &project.id), &project)?;

    let (bundles, dropped_bundles) = localized_export_bundles(&export, &project)?;
    for bundle in &bundles {
        write_json(&bundle_path(&target_root, &bundle.id), bundle)?;
    }
    let history_count = crate::history::append_history_events(
        &target_root,
        &project.id,
        &export.decoded_history_events(&project.id),
    )?;

    let selected_bundle_id = select_active_bundle(&bundles, active_bundle)?;
    let mut remotes = BTreeMap::new();
    remotes.insert(
        remote_name.clone(),
        KnitRemote {
            url: remote.url.clone(),
            token: stored_token,
        },
    );
    let config = KnitConfig {
        schema_version: SCHEMA_VERSION.to_string(),
        active_bundle: selected_bundle_id.clone(),
        active_project: Some(project.id.clone()),
        sync_remote: Some(remote_name.clone()),
        sync_remotes: vec![remote_name.clone()],
        advice: true,
        stealth: None,
        auto_tag: None,
        push_sync: true,
        remotes,
    };
    crate::store::save_config(&target_root, &config)?;

    // Best-effort: restore the cloning user's saved views for the project.
    match super::pull::pull_views_into(&target_root, &remote, &token, &project.id) {
        Ok(count) if count > 0 => {
            crate::human!("{} {count} view(s)", out::heading("Views:"))
        }
        _ => {}
    }

    let mut worktrees_materialized = false;
    if materialize {
        if let Some(bundle_id) = selected_bundle_id.as_deref() {
            materialize_imported_bundle(&target_root, bundle_id)?;
            worktrees_materialized = true;
        }
    }

    crate::human!(
        "{} {} {}",
        out::movement("cloned"),
        out::repo(&project.id),
        out::path(target_root.display())
    );
    crate::human!(
        "{} {} repo(s), {} bundle(s)",
        out::heading("Imported:"),
        project.repos.len(),
        bundles.len()
    );
    if history_count > 0 {
        crate::human!("{} {} event(s)", out::heading("History:"), history_count);
    }
    if !failed_repos.is_empty() {
        crate::human!(
            "{} {} repo(s) could not be cloned and were left out of the workspace:",
            out::heading("Skipped:"),
            failed_repos.len()
        );
        for (local_id, error) in &failed_repos {
            crate::human!("  {}: {}", out::repo(local_id), out::muted(error));
        }
    }
    if let Some(omitted) = export.omitted_repository_count.filter(|count| *count > 0) {
        crate::human!(
            "{} the export omitted {omitted} private repo(s) this token cannot see; the cloned project is incomplete. Ask a project maintainer for access.",
            out::warn("Not exported:")
        );
    }
    for dropped in &dropped_bundles {
        crate::human!(
            "{} dropped bundle {}: repo {} not cloned",
            out::warn("Dropped:"),
            out::repo(&dropped.id),
            dropped.missing_repos.join(", ")
        );
    }

    Ok(clone_document(
        project_identifier,
        &export,
        &project,
        &target_root,
        &repo_paths,
        &failed_repos,
        &bundles,
        dropped_bundles,
        selected_bundle_id,
        worktrees_materialized,
    ))
}

/// Assemble the `--json` result document from the clone's outcomes. Repos keep
/// the export's order; repos the server withheld appear only in
/// `omittedRepositoryCount` (the export never names them).
#[allow(clippy::too_many_arguments)]
fn clone_document(
    project_identifier: &str,
    export: &RemoteProjectExport,
    project: &KnitProject,
    target_root: &Path,
    repo_paths: &BTreeMap<String, PathBuf>,
    failed_repos: &[(String, String)],
    bundles: &[ChangeGroup],
    dropped_bundles: Vec<DroppedBundle>,
    active_bundle: Option<String>,
    worktrees_materialized: bool,
) -> CloneDocument {
    let (identifier_owner, _slug) = super::client::split_project_identifier(project_identifier);
    let owner = identifier_owner.or_else(|| {
        export
            .project
            .organization
            .as_ref()
            .and_then(|organization| organization.slug.clone())
    });
    let repos = export
        .repositories
        .iter()
        .map(|repository| {
            let local_id = export_repo_local_id(repository);
            if repo_paths.contains_key(&local_id) {
                CloneDocumentRepo {
                    id: local_id,
                    status: "cloned",
                    error: None,
                }
            } else {
                let error = failed_repos
                    .iter()
                    .find(|(failed_id, _)| *failed_id == local_id)
                    .map(|(_, error)| error.clone());
                CloneDocumentRepo {
                    id: local_id,
                    status: "failed",
                    error,
                }
            }
        })
        .collect::<Vec<_>>();
    let cloned_repo_count = repos.iter().filter(|repo| repo.status == "cloned").count();
    let failed_repo_count = repos.len() - cloned_repo_count;

    CloneDocument {
        project: CloneDocumentProject {
            id: project.id.clone(),
            owner,
            slug: export.project.slug.clone(),
        },
        target_path: target_root.display().to_string(),
        repos,
        cloned_repo_count,
        failed_repo_count,
        omitted_repository_count: export.omitted_repository_count.unwrap_or(0),
        bundles: CloneDocumentBundles {
            restored: bundles.iter().map(|bundle| bundle.id.clone()).collect(),
            dropped: dropped_bundles,
        },
        active_bundle,
        worktrees_materialized,
    }
}

/// Resolve the remote endpoint and token exactly as `knit clone` does, tagging
/// each failure with its machine-readable kind: endpoint problems are
/// `noRemote`, missing credentials are `noToken`. Shared with
/// `knit remote projects` so both verbs resolve remotes identically.
pub(super) fn resolve_remote_for_clone_classified(
    remote_name: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
) -> std::result::Result<
    (String, KnitRemote, Option<String>, String),
    (RemoteErrorKind, anyhow::Error),
> {
    let (remote_name, remote, stored_token) = resolve_clone_endpoint(remote_name, url, token)
        .map_err(|error| (RemoteErrorKind::NoRemote, error))?;
    let resolved_token = token
        .map(ToString::to_string)
        .or_else(|| token_from_env(&remote_name))
        .or_else(|| remote.token.clone())
        .context("No remote token configured. Set KNIT_REMOTE_<NAME>_TOKEN or KNIT_REMOTE_TOKEN, pass --token, or configure a stored remote token.")
        .map_err(|error| (RemoteErrorKind::NoToken, error))?;
    Ok((remote_name, remote, stored_token, resolved_token))
}

pub(super) fn resolve_clone_endpoint(
    remote_name: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
) -> Result<(String, KnitRemote, Option<String>)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    // Inside a workspace, the effective config already merges global remotes in.
    // Outside one, fall back to the user-global config so `knit clone` works from
    // any directory, not just an existing Knit workspace.
    let config = match find_knit_root(&cwd) {
        Some(root) => crate::store::load_effective_config(&root).ok(),
        None => crate::store::load_global_config().ok(),
    };
    let requested_name = remote_name.map(slugify).filter(|name| !name.is_empty());
    let configured_name = config
        .as_ref()
        .and_then(|config| configured_sync_remote_names(config).into_iter().next());
    let remote_name = requested_name.or(configured_name).with_context(|| {
        "No remote selected. Pass `--remote <name>` with `--url <url>`, or configure a sync remote first."
    })?;
    let configured = config
        .as_ref()
        .and_then(|config| config.remotes.get(&remote_name).cloned());
    let env_url = std::env::var("KNIT_REMOTE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("KNITHUB_URL").ok())
        .filter(|value| !value.trim().is_empty());
    let remote_url = url
        .map(ToString::to_string)
        .or(env_url)
        .or_else(|| configured.as_ref().map(|remote| remote.url.clone()))
        .with_context(|| {
            format!("No URL configured for remote `{remote_name}`. Pass --url, set KNIT_REMOTE_URL, or run `knit remote add {remote_name} <url>`.")
        })?;
    let stored_token = token
        .map(ToString::to_string)
        .or_else(|| configured.as_ref().and_then(|remote| remote.token.clone()));
    let remote = KnitRemote {
        url: normalize_base_url(&remote_url),
        token: stored_token.clone(),
    };

    Ok((remote_name, remote, stored_token))
}

fn resolve_clone_target(target: Option<&Path>, project_identifier: &str) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    // Default the directory to the project slug, dropping any `owner/` prefix.
    let (_owner, slug) = super::client::split_project_identifier(project_identifier);
    let target = target
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(slug));
    if target.is_absolute() {
        Ok(target)
    } else {
        Ok(cwd.join(target))
    }
}

fn prepare_clone_target(target: &Path) -> Result<()> {
    if target.join(".knit/config.json").exists() {
        bail!("{} is already a Knit workspace.", target.display());
    }

    if target.exists() {
        let mut entries = fs::read_dir(target)
            .with_context(|| format!("failed to read clone target {}", target.display()))?;
        if entries.next().transpose()?.is_some() {
            bail!("Clone target {} is not empty.", target.display());
        }
    } else {
        fs::create_dir_all(target)
            .with_context(|| format!("failed to create clone target {}", target.display()))?;
    }

    Ok(())
}

pub(super) fn clone_export_repositories(
    target_root: &Path,
    repositories: &[RemoteExportRepository],
) -> Result<BTreeMap<String, PathBuf>> {
    let mut paths = BTreeMap::new();

    for repository in repositories {
        let (local_id, repo_path) = clone_one_export_repository(target_root, repository)?;
        paths.insert(local_id, repo_path);
    }

    Ok(paths)
}

/// Clone (or adopt) one exported repository. Authentication comes from the
/// installed Git credential helpers; a failed non-public clone carries the
/// access hint.
fn clone_one_export_repository(
    target_root: &Path,
    repository: &RemoteExportRepository,
) -> Result<(String, PathBuf)> {
    let local_id = export_repo_local_id(repository);
    let remote_url = repository
        .remote_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .with_context(|| format!("{local_id}: remote export has no clone URL."))?;
    let repo_path = target_root.join(&local_id);

    if repo_path.exists() {
        if !is_git_worktree(&repo_path) {
            bail!("{} exists but is not a git checkout.", repo_path.display());
        }
        crate::human!(
            "{}: {} {}",
            out::repo(&local_id),
            out::muted("using existing checkout"),
            out::path(repo_path.display())
        );
        checkout_export_base_branch(&repo_path, repository)?;
        return Ok((local_id, repo_path));
    }

    let clone_args = [
        OsString::from("clone"),
        OsString::from(remote_url),
        repo_path.as_os_str().to_os_string(),
    ];
    if let Err(error) = git_output(target_root, clone_args) {
        // A failed clone can leave a partial target dir behind; clear it so a
        // rerun starts clean.
        if repo_path.exists() {
            let _ = fs::remove_dir_all(&repo_path);
        }
        let error = if repository.visibility.as_deref() == Some("public") {
            error
        } else {
            anyhow::anyhow!("{error:#}; {NO_ACCESS_HINT}")
        };
        return Err(error.context(format!("{local_id}: failed to clone {remote_url}")));
    }
    crate::human!(
        "{}: {} {}",
        out::repo(&local_id),
        out::movement("cloned"),
        out::path(repo_path.display())
    );

    checkout_export_base_branch(&repo_path, repository)?;
    Ok((local_id, repo_path))
}

/// Clone every exported repository, skipping (and recording) any that fail so an
/// inaccessible repo, such as a private GitHub repo the token cannot read, does
/// not abort the whole clone. Mirrors the per-repo resilience used by incremental
/// remote pull. Returns the cloned paths and the (local id, error) failures.
fn clone_export_repositories_collecting(
    target_root: &Path,
    repositories: &[RemoteExportRepository],
) -> (BTreeMap<String, PathBuf>, Vec<(String, String)>) {
    let mut paths = BTreeMap::new();
    let mut failed = Vec::new();
    for repository in repositories {
        match clone_one_export_repository(target_root, repository) {
            Ok((local_id, repo_path)) => {
                paths.insert(local_id, repo_path);
            }
            Err(error) => failed.push((export_repo_local_id(repository), format!("{error:#}"))),
        }
    }
    (paths, failed)
}

fn format_repo_failures(failed: &[(String, String)]) -> String {
    failed
        .iter()
        .map(|(local_id, error)| format!("  {local_id}: {error}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn checkout_export_base_branch(
    repo_path: &Path,
    repository: &RemoteExportRepository,
) -> Result<()> {
    let Some(base_branch) = repository
        .default_branch
        .as_deref()
        .filter(|branch| !branch.trim().is_empty())
    else {
        return Ok(());
    };
    if current_branch(repo_path)?.as_deref() == Some(base_branch) {
        return Ok(());
    }

    let remote_ref = format!("origin/{base_branch}");
    if ref_exists(repo_path, &remote_ref) {
        git_output(
            repo_path,
            [
                OsString::from("checkout"),
                OsString::from("-B"),
                OsString::from(base_branch),
                OsString::from(remote_ref),
            ],
        )?;
    }
    Ok(())
}

fn local_project_from_export(
    export: &RemoteProjectExport,
    repo_paths: &BTreeMap<String, PathBuf>,
) -> Result<KnitProject> {
    let mut project = export
        .knit_project
        .clone()
        .unwrap_or_else(|| KnitProject::new(export.project.slug.clone(), now_iso()));
    project.id = slugify(&project.id);
    project.repos.clear();

    for repository in &export.repositories {
        let local_id = export_repo_local_id(repository);
        // Repos that failed to clone are absent from repo_paths; leave them out
        // of the local project rather than recording an entry with no checkout.
        let Some(repo_path) = repo_paths.get(&local_id) else {
            continue;
        };
        project
            .repos
            .push(project_repo_entry_from_export(repository, repo_path));
    }

    project.updated_at = now_iso();
    Ok(project)
}

/// Build a local project repo entry from an exported repository and its cloned
/// path. Shared with incremental remote pull so both code paths record repos the
/// same way.
pub(super) fn project_repo_entry_from_export(
    repository: &RemoteExportRepository,
    repo_path: &Path,
) -> ProjectRepoEntry {
    ProjectRepoEntry {
        id: export_repo_local_id(repository),
        path: repo_path.to_string_lossy().to_string(),
        remote: repository.remote_url.clone(),
        base_branch: repository
            .default_branch
            .clone()
            .filter(|branch| !branch.trim().is_empty())
            .unwrap_or_else(|| "main".to_string()),
        // Remote metadata is advisory; anything other than an explicit
        // `inPlace` falls back to the worktree default.
        checkout_mode: match metadata_string(&repository.metadata, "checkoutMode").as_deref() {
            Some("inPlace") => CheckoutMode::InPlace,
            _ => CheckoutMode::Worktree,
        },
        include_by_default: metadata_bool(&repository.metadata, "includeByDefault").unwrap_or(true),
    }
}

/// Localize every exportable bundle onto the local project, dropping any bundle
/// that references a repo missing from the project (because its clone failed or
/// the server withheld it). Returns the localized bundles plus a record of each
/// dropped bundle and the repo ids it was missing, so callers can surface the
/// loss instead of silently presenting a partial import.
fn localized_export_bundles(
    export: &RemoteProjectExport,
    project: &KnitProject,
) -> Result<(Vec<ChangeGroup>, Vec<DroppedBundle>)> {
    let available: BTreeSet<&str> = project.repos.iter().map(|repo| repo.id.as_str()).collect();
    let mut localized = Vec::new();
    let mut dropped = Vec::new();

    for bundle in export
        .bundles
        .iter()
        .filter(|bundle| bundle.lifecycle_state != "deleted")
    {
        let Some(artifact) = bundle.current_artifact.as_ref() else {
            continue;
        };
        let payload = decode_bundle_payload(&artifact.payload, &bundle.slug)?;
        let missing_repos = missing_bundle_repos(&payload, &available);
        if !missing_repos.is_empty() {
            dropped.push(DroppedBundle {
                id: bundle.slug.clone(),
                missing_repos,
            });
            continue;
        }
        localized.push(localize_bundle(payload, project)?);
    }

    Ok((localized, dropped))
}

/// Repo ids a bundle payload references that are absent from the cloned
/// project, in payload order.
fn missing_bundle_repos(payload: &ChangeGroup, available: &BTreeSet<&str>) -> Vec<String> {
    payload
        .repos
        .iter()
        .filter(|repo| !available.contains(repo.id.as_str()))
        .map(|repo| repo.id.clone())
        .collect()
}

fn select_active_bundle(
    bundles: &[ChangeGroup],
    requested: Option<&str>,
) -> Result<Option<String>> {
    if let Some(requested) = requested {
        let requested = slugify(requested);
        if bundles.iter().any(|bundle| bundle.id == requested) {
            return Ok(Some(requested));
        }
        bail!("Remote export has no bundle named `{requested}`.");
    }

    Ok(bundles
        .iter()
        .find(|bundle| {
            bundle.state.unwrap_or(crate::model::BundleState::Open)
                == crate::model::BundleState::Open
        })
        .or_else(|| bundles.first())
        .map(|bundle| bundle.id.clone()))
}

pub(super) fn materialize_imported_bundle(root: &Path, bundle_id: &str) -> Result<()> {
    let bundle_path = bundle_path(root, bundle_id);
    let bundle: ChangeGroup = read_json(&bundle_path)?;
    prepare_feature_branches(&bundle)?;
    let mut active = ActiveBundle::unlocked(root.to_path_buf(), bundle_path, bundle);
    materialize_repos(&mut active, None)?;
    fast_forward_feature_checkouts(&mut active)?;
    let bundle_agents = write_bundle_worktree_agents_md(&active)?;
    print_bundle_worktree_agents_summary(bundle_agents.as_deref());
    crate::store::save_active_bundle(&active)
}

pub(super) fn export_repo_local_id(repository: &RemoteExportRepository) -> String {
    repository
        .local_id
        .clone()
        .or_else(|| metadata_string(&repository.metadata, "localId"))
        .unwrap_or_else(|| slugify(&repository.name))
}

fn metadata_string(metadata: &Value, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
}

fn metadata_bool(metadata: &Value, key: &str) -> Option<bool> {
    metadata.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("knit-clone-test-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_source_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--allow-empty", "-q", "-m", "init"]);
    }

    fn export_repo(name: &str, remote_url: &str) -> RemoteExportRepository {
        RemoteExportRepository {
            local_id: Some(name.to_string()),
            name: name.to_string(),
            default_branch: None,
            remote_url: Some(remote_url.to_string()),
            visibility: None,
            metadata: Value::Null,
        }
    }

    #[test]
    fn clone_collecting_skips_failed_repos_and_keeps_the_good_ones() {
        let root = temp_dir("collect");
        let source = root.join("source.git");
        init_source_repo(&source);
        let target = root.join("workspace");
        fs::create_dir_all(&target).unwrap();

        // `bad` points at a path that cannot be cloned; `good` is a real repo.
        let repos = [
            export_repo("bad", &root.join("does-not-exist").to_string_lossy()),
            export_repo("good", &source.to_string_lossy()),
        ];

        let (paths, failed) = clone_export_repositories_collecting(&target, &repos);

        assert!(paths.contains_key("good"), "good repo should be cloned");
        assert!(target.join("good").join(".git").exists());
        assert!(!paths.contains_key("bad"), "bad repo should be skipped");
        assert!(!target.join("bad").exists());

        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "bad");
        assert!(
            failed[0].1.contains("bad") || failed[0].1.contains("clone"),
            "failure should name the repo or the clone step: {}",
            failed[0].1
        );

        fs::remove_dir_all(root).unwrap();
    }

    fn export_bundle_payload(id: &str, repo_ids: &[&str]) -> Value {
        let repos: Vec<Value> = repo_ids
            .iter()
            .map(|repo_id| {
                serde_json::json!({
                    "id": repo_id,
                    "path": format!("/tmp/{repo_id}"),
                    "baseBranch": "main",
                })
            })
            .collect();
        serde_json::json!({
            "schemaVersion": "1",
            "kind": "knit.bundle",
            "id": id,
            "title": id,
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "repos": repos,
            "commitGroups": [],
        })
    }

    fn export_with_bundles(bundles: Value) -> RemoteProjectExport {
        serde_json::from_value(serde_json::json!({
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": bundles,
            "historyEvents": [],
        }))
        .unwrap()
    }

    fn project_with_backend() -> KnitProject {
        let mut project = KnitProject::new("demo".to_string(), now_iso());
        project.repos.push(ProjectRepoEntry {
            id: "backend".to_string(),
            path: "/tmp/backend".to_string(),
            remote: None,
            base_branch: "main".to_string(),
            checkout_mode: CheckoutMode::Worktree,
            include_by_default: true,
        });
        project
    }

    #[test]
    fn localized_export_bundles_records_dropped_bundles_with_missing_repos() {
        let export = export_with_bundles(serde_json::json!([
            {
                "id": "rb-1",
                "slug": "feature-a",
                "lifecycleState": "open",
                "currentArtifact": {
                    "artifactHash": "hash-a",
                    "payload": export_bundle_payload("feature-a", &["backend"]),
                },
            },
            {
                "id": "rb-2",
                "slug": "feature-c",
                "lifecycleState": "open",
                "currentArtifact": {
                    "artifactHash": "hash-c",
                    "payload": export_bundle_payload("feature-c", &["backend", "frontend"]),
                },
            },
            // A deleted bundle and an artifact-less bundle are ignored, not dropped.
            {"id": "rb-3", "slug": "gone", "lifecycleState": "deleted", "currentArtifact": null},
            {"id": "rb-4", "slug": "empty", "lifecycleState": "open", "currentArtifact": null},
        ]));
        let project = project_with_backend();

        let (localized, dropped) = localized_export_bundles(&export, &project).unwrap();

        assert_eq!(localized.len(), 1);
        assert_eq!(localized[0].id, "feature-a");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].id, "feature-c");
        assert_eq!(dropped[0].missing_repos, vec!["frontend".to_string()]);
    }

    #[test]
    fn clone_document_serializes_to_the_contract_shape() {
        let document = CloneDocument {
            project: CloneDocumentProject {
                id: "knit-tools".to_string(),
                owner: Some("marc-merino".to_string()),
                slug: "knit-tools".to_string(),
            },
            target_path: "/abs/path/to/knit-tools".to_string(),
            repos: vec![
                CloneDocumentRepo {
                    id: "backend".to_string(),
                    status: "cloned",
                    error: None,
                },
                CloneDocumentRepo {
                    id: "frontend".to_string(),
                    status: "failed",
                    error: Some("git clone failed".to_string()),
                },
            ],
            cloned_repo_count: 1,
            failed_repo_count: 1,
            omitted_repository_count: 1,
            bundles: CloneDocumentBundles {
                restored: vec!["feature-a".to_string()],
                dropped: vec![DroppedBundle {
                    id: "feature-c".to_string(),
                    missing_repos: vec!["frontend".to_string()],
                }],
            },
            active_bundle: Some("feature-a".to_string()),
            worktrees_materialized: true,
        };

        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "project": {"id": "knit-tools", "owner": "marc-merino", "slug": "knit-tools"},
                "targetPath": "/abs/path/to/knit-tools",
                "repos": [
                    {"id": "backend", "status": "cloned"},
                    {"id": "frontend", "status": "failed", "error": "git clone failed"},
                ],
                "clonedRepoCount": 1,
                "failedRepoCount": 1,
                "omittedRepositoryCount": 1,
                "bundles": {
                    "restored": ["feature-a"],
                    "dropped": [{"id": "feature-c", "missingRepos": ["frontend"]}],
                },
                "activeBundle": "feature-a",
                "worktreesMaterialized": true,
            })
        );
    }

    #[test]
    fn format_repo_failures_lists_each_repo() {
        let failures = vec![
            ("backend".to_string(), "Repository not found".to_string()),
            ("frontend".to_string(), "permission denied".to_string()),
        ];
        let text = format_repo_failures(&failures);
        assert!(text.contains("backend: Repository not found"));
        assert!(text.contains("frontend: permission denied"));
    }
}
