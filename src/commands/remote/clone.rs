//! `knit clone` — import a KnitHub project export into a fresh local workspace:
//! clone its repositories, write the project and bundle artifacts, and
//! optionally materialize the active bundle.

use super::client::{
    decode_bundle_payload, fast_forward_feature_checkouts, fetch_project_export, localize_bundle,
    normalize_base_url, prepare_feature_branches, token_from_env,
};
use super::{RemoteExportRepository, RemoteProjectExport};
use crate::commands::agents::{print_worktree_agents_summary, write_worktree_agents_md};
use crate::commands::worktree::materialize_repos;
use crate::git::{current_branch, git_output, is_git_worktree, ref_exists};
use crate::ids::slugify;
use crate::model::{
    ChangeGroup, KnitConfig, KnitProject, KnitRemote, ProjectRepoEntry, CHECKOUT_MODE_WORKTREE,
    SCHEMA_VERSION,
};
use crate::output as out;
use crate::store::{bundle_path, find_knit_root, load_config, project_path, read_json, write_json, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub fn clone_project_from_remote(
    project_identifier: &str,
    target: Option<&Path>,
    remote_name: &str,
    url: Option<&str>,
    token: Option<&str>,
    active_bundle: Option<&str>,
    materialize: bool,
) -> Result<()> {
    let remote_name = slugify(remote_name);
    let (remote, stored_token, token) = resolve_remote_for_clone(&remote_name, url, token)?;
    let export = fetch_project_export(&remote, &token, project_identifier)?;
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

    let repo_paths = clone_export_repositories(&target_root, &export.repositories)?;
    let project = local_project_from_export(&export, &repo_paths)?;
    write_json(&project_path(&target_root, &project.id), &project)?;

    let bundles = localized_export_bundles(&export, &project)?;
    for bundle in &bundles {
        write_json(&bundle_path(&target_root, &bundle.id), bundle)?;
    }

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
        advice: true,
        push_sync: true,
        remotes,
    };
    crate::store::save_config(&target_root, &config)?;

    // Best-effort: restore the cloning user's saved views for the project.
    match super::pull::pull_views_into(&target_root, &remote, &token, &project.id) {
        Ok(count) if count > 0 => {
            println!("{} {count} view(s)", out::heading("Views:"))
        }
        _ => {}
    }

    if materialize {
        if let Some(bundle_id) = selected_bundle_id.as_deref() {
            materialize_imported_bundle(&target_root, bundle_id)?;
        }
    }

    println!(
        "{} {} {}",
        out::movement("cloned"),
        out::repo(&project.id),
        out::path(target_root.display())
    );
    println!(
        "{} {} repo(s), {} bundle(s)",
        out::heading("Imported:"),
        project.repos.len(),
        bundles.len()
    );
    Ok(())
}

fn resolve_remote_for_clone(
    remote_name: &str,
    url: Option<&str>,
    token: Option<&str>,
) -> Result<(KnitRemote, Option<String>, String)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let configured = find_knit_root(&cwd)
        .and_then(|root| load_config(&root).ok())
        .and_then(|config| config.remotes.get(remote_name).cloned());
    let remote_url = url
        .map(ToString::to_string)
        .or_else(|| std::env::var("KNITHUB_URL").ok())
        .or_else(|| configured.as_ref().map(|remote| remote.url.clone()))
        .with_context(|| {
            format!("No KnitHub URL configured. Pass --url, set KNITHUB_URL, or run `knit remote add {remote_name} <url>` from an existing workspace.")
        })?;
    let stored_token = token
        .map(ToString::to_string)
        .or_else(|| configured.as_ref().and_then(|remote| remote.token.clone()));
    let remote = KnitRemote {
        url: normalize_base_url(&remote_url),
        token: stored_token.clone(),
    };
    let resolved_token = token
        .map(ToString::to_string)
        .or_else(|| token_from_env(remote_name))
        .or_else(|| remote.token.clone())
        .context("No KnitHub token configured. Set KNITHUB_TOKEN, KNIT_REMOTE_<NAME>_TOKEN, pass --token, or configure a stored remote token.")?;

    Ok((remote, stored_token, resolved_token))
}

fn resolve_clone_target(target: Option<&Path>, project_identifier: &str) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let target = target
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(slugify(project_identifier)));
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
            println!(
                "{}: {} {}",
                out::repo(&local_id),
                out::muted("using existing checkout"),
                out::path(repo_path.display())
            );
        } else {
            git_output(
                target_root,
                [
                    OsString::from("clone"),
                    OsString::from(remote_url),
                    repo_path.as_os_str().to_os_string(),
                ],
            )
            .with_context(|| format!("{local_id}: failed to clone {remote_url}"))?;
            println!(
                "{}: {} {}",
                out::repo(&local_id),
                out::movement("cloned"),
                out::path(repo_path.display())
            );
        }

        checkout_export_base_branch(&repo_path, repository)?;
        paths.insert(local_id, repo_path);
    }

    Ok(paths)
}

fn checkout_export_base_branch(repo_path: &Path, repository: &RemoteExportRepository) -> Result<()> {
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
        let repo_path = repo_paths
            .get(&local_id)
            .with_context(|| format!("{local_id}: repository was not cloned"))?;
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
        checkout_mode: metadata_string(&repository.metadata, "checkoutMode")
            .unwrap_or_else(|| CHECKOUT_MODE_WORKTREE.to_string()),
        include_by_default: metadata_bool(&repository.metadata, "includeByDefault").unwrap_or(true),
    }
}

fn localized_export_bundles(
    export: &RemoteProjectExport,
    project: &KnitProject,
) -> Result<Vec<ChangeGroup>> {
    export
        .bundles
        .iter()
        .filter(|bundle| bundle.lifecycle_state != "deleted")
        .filter_map(|bundle| {
            bundle
                .current_artifact
                .as_ref()
                .map(|artifact| (bundle, artifact))
        })
        .map(|(bundle, artifact)| {
            let payload = decode_bundle_payload(&artifact.payload, &bundle.slug)?;
            localize_bundle(payload, project)
        })
        .collect()
}

fn select_active_bundle(bundles: &[ChangeGroup], requested: Option<&str>) -> Result<Option<String>> {
    if let Some(requested) = requested {
        let requested = slugify(requested);
        if bundles.iter().any(|bundle| bundle.id == requested) {
            return Ok(Some(requested));
        }
        bail!("Remote export has no bundle named `{requested}`.");
    }

    Ok(bundles
        .iter()
        .find(|bundle| bundle.state.as_deref().unwrap_or("open") == "open")
        .or_else(|| bundles.first())
        .map(|bundle| bundle.id.clone()))
}

fn materialize_imported_bundle(root: &Path, bundle_id: &str) -> Result<()> {
    let bundle_path = bundle_path(root, bundle_id);
    let bundle: ChangeGroup = read_json(&bundle_path)?;
    prepare_feature_branches(&bundle)?;
    let mut active = ActiveBundle::unlocked(root.to_path_buf(), bundle_path, bundle);
    materialize_repos(&mut active, None)?;
    fast_forward_feature_checkouts(&mut active)?;
    let worktree_agents = write_worktree_agents_md(&active)?;
    print_worktree_agents_summary(&worktree_agents);
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
