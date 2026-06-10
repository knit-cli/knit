//! Remote configuration commands (add/list/show/remove/token) and pushing the
//! local project and bundle artifact to a KnitHub remote, including the implicit
//! sync-on-push.

use super::client::{
    configured_sync_remote_names, decode_response, effective_workspace_config,
    load_project_if_present, normalize_base_url, request, request_json, resolve_project_id,
    resolve_remote, resolve_sync_remote_names, resolve_token, token_from_env, workspace_config,
};
use super::{RemoteArtifact, RemoteBundle, RemoteProject};
use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitConfig, KnitProject, KnitRemote, ProjectRepoEntry};
use crate::output as out;
use crate::store::{
    find_knit_root, load_active_bundle, load_config, load_effective_config, load_global_config,
    save_config, save_global_config,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub fn add_remote(name: &str, url: &str, token: Option<&str>, global: bool) -> Result<()> {
    let (root, mut config) = if global {
        (None, load_global_config()?)
    } else {
        let (root, config) = workspace_config()?;
        (Some(root), config)
    };
    let remote_name = slugify(name);
    config.remotes.insert(
        remote_name.clone(),
        KnitRemote {
            url: normalize_base_url(url),
            token: token.map(ToString::to_string),
        },
    );
    if let Some(root) = root {
        save_config(&root, &config)?;
    } else {
        save_global_config(&config)?;
    }
    let scope = if global { "global " } else { "" };
    println!(
        "{} {}{}",
        out::movement("configured"),
        scope,
        out::repo(remote_name)
    );
    Ok(())
}

pub fn list_remotes(global: bool) -> Result<()> {
    let (config, sources) = remote_listing(global)?;
    if config.remotes.is_empty() {
        println!("{}", out::muted("No KnitHub remotes configured."));
        return Ok(());
    }

    let sync_remotes = configured_sync_remote_names(&config);
    for (name, remote) in config.remotes {
        let source = sources
            .get(&name)
            .map(String::as_str)
            .unwrap_or("workspace");
        let token_label = if token_from_env(&name).is_some() {
            "token from env"
        } else if remote.token.is_some() {
            "stored token"
        } else {
            "no token"
        };
        let sync_label = sync_remotes
            .contains(&name)
            .then_some("sync")
            .unwrap_or("not sync");
        println!(
            "{} {} {} {} {}",
            out::repo(name),
            remote.url,
            out::muted(source),
            out::muted(token_label),
            out::muted(sync_label)
        );
    }
    Ok(())
}

pub fn show_remote(name: &str, global: bool) -> Result<()> {
    let (config, sources) = remote_listing(global)?;
    let remote_name = slugify(name);
    let remote = config
        .remotes
        .get(&remote_name)
        .with_context(|| format!("No KnitHub remote named `{remote_name}`."))?;
    let sync_remotes = configured_sync_remote_names(&config);
    println!("{} {}", out::heading("Remote:"), out::repo(&remote_name));
    println!("{} {}", out::heading("URL:"), remote.url);
    println!(
        "{} {}",
        out::heading("Scope:"),
        sources
            .get(&remote_name)
            .map(String::as_str)
            .unwrap_or("workspace")
    );
    println!(
        "{} {}",
        out::heading("Sync:"),
        if sync_remotes.contains(&remote_name) {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "{} {}",
        out::heading("Token:"),
        if token_from_env(&remote_name).is_some() {
            "from environment"
        } else if remote.token.is_some() {
            "stored"
        } else {
            "not configured"
        }
    );
    Ok(())
}

pub fn remove_remote(name: &str, global: bool) -> Result<()> {
    let (root, mut config) = if global {
        (None, load_global_config()?)
    } else {
        let (root, config) = workspace_config()?;
        (Some(root), config)
    };
    let remote_name = slugify(name);
    if config.remotes.remove(&remote_name).is_none() {
        bail!("No KnitHub remote named `{remote_name}`.");
    }
    config
        .sync_remotes
        .retain(|entry| slugify(entry) != remote_name);
    let removed_sync_remote = config
        .sync_remote
        .as_deref()
        .map(slugify)
        .unwrap_or_default()
        == remote_name;
    if removed_sync_remote {
        config.sync_remote = config.sync_remotes.first().cloned();
    }
    if let Some(root) = root {
        save_config(&root, &config)?;
    } else {
        save_global_config(&config)?;
    }
    let scope = if global { "global " } else { "" };
    println!(
        "{} {}{}",
        out::movement("removed"),
        scope,
        out::repo(remote_name)
    );
    Ok(())
}

pub fn set_remote_token(name: &str, token: Option<&str>, clear: bool, global: bool) -> Result<()> {
    let (root, mut config) = if global {
        (None, load_global_config()?)
    } else {
        let (root, config) = workspace_config()?;
        (Some(root), config)
    };
    let remote_name = slugify(name);
    let remote = config
        .remotes
        .get_mut(&remote_name)
        .with_context(|| format!("No KnitHub remote named `{remote_name}`."))?;

    if clear {
        remote.token = None;
        println!("{} {}", out::movement("cleared"), out::repo(remote_name));
    } else {
        let token = token.context("Pass a token value or use --clear.")?;
        remote.token = Some(token.to_string());
        println!("{} {}", out::movement("stored"), out::repo(remote_name));
    }

    if let Some(root) = root {
        save_config(&root, &config)?;
    } else {
        save_global_config(&config)?;
    }
    Ok(())
}

pub fn push_project_to_remote(name: Option<&str>, remote_name: &str) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let project_id = resolve_project_id(&root, &config, name)?;
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let project = load_project_if_present(&root, &project_id)?;
    let pushed = upsert_project(remote, &token, &project_id, project.as_ref())?;
    let repo_count = match project.as_ref() {
        Some(project) => push_repositories(remote, &token, &pushed.slug, &project.repos)?,
        None => 0,
    };

    println!(
        "{} {} {} {}",
        out::movement("pushed"),
        out::repo(&pushed.slug),
        out::muted(&pushed.id),
        out::muted(format!("{repo_count} repo(s)"))
    );

    // Best-effort: also upload the user's saved views for this project. A server
    // without the views endpoint must not fail the project push.
    if let Err(error) = upload_views(remote, &token, &root, &pushed.slug) {
        println!("{} {error:#}", out::warn("views not synced:"));
    }
    Ok(())
}

/// Upload the local saved views for a project to the remote, if any exist.
fn upload_views(remote: &KnitRemote, token: &str, root: &Path, project_slug: &str) -> Result<()> {
    let views = crate::store::load_views(root, project_slug)?;
    if views.views.is_empty() && views.default_view.is_none() {
        return Ok(());
    }
    let payload = json!({
        "defaultView": views.default_view,
        "views": views.views,
    });
    request_json::<Value>(
        remote,
        token,
        "PUT",
        &format!("/projects/{project_slug}/view"),
        Some(&payload),
    )?;
    Ok(())
}

/// Push the current user's saved views for a project to the KnitHub remote.
pub fn push_views_to_remote(name: Option<&str>, remote_name: &str) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let project_id = resolve_project_id(&root, &config, name)?;
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let views = crate::store::load_views(&root, &project_id)?;
    let payload = json!({
        "defaultView": views.default_view,
        "views": views.views,
    });
    request_json::<Value>(
        remote,
        &token,
        "PUT",
        &format!("/projects/{project_id}/view"),
        Some(&payload),
    )?;
    println!(
        "{} {} {}",
        out::movement("pushed views"),
        out::repo(&project_id),
        out::muted(format!("{} view(s)", views.views.len()))
    );
    Ok(())
}

pub fn push_bundle_to_remote(remote_name: &str, project: Option<&str>) -> Result<()> {
    let active = load_active_bundle()?;
    let config = load_effective_config(&active.root)?;
    let project_id = project
        .map(slugify)
        .or_else(|| active.bundle.project_id.clone())
        .or_else(|| config.active_project.clone())
        .context("No project selected. Pass --project or run `knit init <name>`.")?;
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let local_project = load_project_if_present(&active.root, &project_id)?;
    let pushed_project = upsert_project(remote, &token, &project_id, local_project.as_ref())?;
    if let Some(project) = local_project.as_ref() {
        push_repositories(remote, &token, &pushed_project.slug, &project.repos)?;
    }

    let pushed_bundle = upsert_bundle(remote, &token, &pushed_project.slug, &active.bundle)?;
    let artifact = push_bundle_artifact(remote, &token, &pushed_bundle.id, &active.bundle)?;
    let history_result = super::history::push_project_history_events(
        remote,
        &token,
        &pushed_project.slug,
        &active.root,
        &project_id,
    );

    println!(
        "{} {} -> {}",
        out::movement("pushed"),
        out::repo(&active.bundle.id),
        out::repo(&pushed_bundle.slug)
    );
    println!(
        "{} {} {}",
        out::heading("Artifact:"),
        artifact.id,
        out::muted(artifact.artifact_hash)
    );
    match history_result {
        Ok(history_count) if history_count > 0 => {
            println!(
                "{} {}",
                out::heading("History:"),
                out::muted(format!("{history_count} event(s) synced"))
            );
        }
        Ok(_) => {}
        Err(error) => {
            println!("{} {error:#}", out::warn("History sync skipped:"));
        }
    }
    Ok(())
}

/// Push the resolved bundle artifact to configured KnitHub remote(s) alongside a
/// git push, when enabled.
///
/// Resolution order for remotes: repeated explicit `--remote`, then
/// `syncRemotes`, then legacy `sync_remote`, then a remote literally named
/// `knithub`. With no remote configured this is a silent no-op. The `push_sync`
/// config disables implicit sync, but explicit `--remote` still forces it.
/// `--no-remote` always skips. Sync failures are reported as warnings and never
/// fail the git push that already succeeded.
pub fn maybe_sync_bundle_to_remote(remote_overrides: &[String], no_remote: bool) -> Result<()> {
    if no_remote {
        return Ok(());
    }
    let Ok((_, config)) = effective_workspace_config() else {
        return Ok(());
    };
    if !config.push_sync && remote_overrides.is_empty() {
        return Ok(());
    }
    let remote_names = resolve_sync_remote_names(&config, remote_overrides);
    if remote_names.is_empty() {
        return Ok(());
    }
    let explicit = !remote_overrides.is_empty();
    let multiple = remote_names.len() > 1;

    for remote_name in remote_names {
        if let Err(error) = resolve_remote(&config, &remote_name) {
            // An explicitly requested remote that does not exist is an error; an
            // implicit configured remote is skipped as best-effort sync.
            if explicit {
                return Err(error);
            }
            println!(
                "{} {error:#}",
                out::warn(format!("KnitHub sync skipped ({remote_name}):"))
            );
            continue;
        }
        if multiple {
            println!("{} {}", out::heading("Remote:"), out::repo(&remote_name));
        }
        if let Err(error) = push_bundle_to_remote(&remote_name, None) {
            println!(
                "{} {error:#}",
                out::warn(format!("KnitHub sync skipped ({remote_name}):"))
            );
        }
    }
    Ok(())
}

/// Push the resolved bundle artifact to KnitHub when push-sync is enabled.
///
/// Unlike `maybe_sync_bundle_to_remote`, sync failures are returned to the
/// caller. This is used after landing so a stale remote lifecycle state is
/// visible instead of being hidden behind a best-effort warning.
pub fn sync_bundle_to_remote_if_enabled(
    remote_overrides: &[String],
    no_remote: bool,
) -> Result<()> {
    if no_remote {
        return Ok(());
    }
    let Ok((_, config)) = effective_workspace_config() else {
        return Ok(());
    };
    if !config.push_sync && remote_overrides.is_empty() {
        return Ok(());
    }
    let remote_names = resolve_sync_remote_names(&config, remote_overrides);
    if remote_names.is_empty() {
        return Ok(());
    }

    sync_bundle_to_remote_names(&config, &remote_names)
}

fn sync_bundle_to_remote_names(config: &KnitConfig, remote_names: &[String]) -> Result<()> {
    let multiple = remote_names.len() > 1;
    let mut failures = Vec::new();
    for remote_name in remote_names {
        if let Err(error) = resolve_remote(config, remote_name) {
            failures.push(format!("{remote_name}: {error:#}"));
            continue;
        }
        if multiple {
            println!("{} {}", out::heading("Remote:"), out::repo(remote_name));
        }
        if let Err(error) = push_bundle_to_remote(remote_name, None) {
            failures.push(format!("{remote_name}: {error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "KnitHub sync failed for {} remote(s):\n{}",
            failures.len(),
            failures.join("\n")
        )
    }
}

fn upsert_project(
    remote: &KnitRemote,
    token: &str,
    project_id: &str,
    project: Option<&KnitProject>,
) -> Result<RemoteProject> {
    let payload = project_payload(project_id, project);
    let path = format!("/projects/{project_id}");
    let response = request(remote, token, "PATCH", &path, Some(&payload))?;
    if response.status == 404 {
        decode_response(request(remote, token, "POST", "/projects", Some(&payload))?)
    } else {
        decode_response(response)
    }
}

pub(super) fn upsert_project_for_history(
    remote: &KnitRemote,
    token: &str,
    project_id: &str,
    project: Option<&KnitProject>,
) -> Result<RemoteProject> {
    upsert_project(remote, token, project_id, project)
}

fn push_repositories(
    remote: &KnitRemote,
    token: &str,
    project_slug: &str,
    repos: &[ProjectRepoEntry],
) -> Result<usize> {
    for repo in repos {
        let payload = repository_payload(repo);
        request_json::<Value>(
            remote,
            token,
            "POST",
            &format!("/projects/{project_slug}/repositories"),
            Some(&payload),
        )?;
    }
    Ok(repos.len())
}

pub(super) fn push_repositories_for_history(
    remote: &KnitRemote,
    token: &str,
    project_slug: &str,
    repos: &[ProjectRepoEntry],
) -> Result<usize> {
    push_repositories(remote, token, project_slug, repos)
}

fn upsert_bundle(
    remote: &KnitRemote,
    token: &str,
    project_slug: &str,
    bundle: &ChangeGroup,
) -> Result<RemoteBundle> {
    let payload = bundle_payload(bundle);
    request_json(
        remote,
        token,
        "POST",
        &format!("/projects/{project_slug}/bundles"),
        Some(&payload),
    )
}

fn push_bundle_artifact(
    remote: &KnitRemote,
    token: &str,
    bundle_id: &str,
    bundle: &ChangeGroup,
) -> Result<RemoteArtifact> {
    let payload = json!({
        "kind": "bundle",
        "schema_version": bundle.schema_version,
        "producer": "knit",
        "producer_version": env!("CARGO_PKG_VERSION"),
        "payload": bundle
    });
    request_json(
        remote,
        token,
        "POST",
        &format!("/bundles/{bundle_id}/artifacts"),
        Some(&payload),
    )
}

fn project_payload(project_id: &str, project: Option<&KnitProject>) -> Value {
    let repo_count = project
        .map(|project| project.repos.len())
        .unwrap_or_default();
    let (schema_version, kind) = project
        .map(|project| (project.schema_version.as_str(), project.kind.as_str()))
        .unwrap_or((crate::model::SCHEMA_VERSION, "KnitProject"));
    let mut metadata = json!({
        "schemaVersion": schema_version,
        "kind": kind,
        "repoCount": repo_count,
        "pushedBy": "knit"
    });
    if let Some(project) = project {
        metadata["knitProject"] = serde_json::to_value(project).unwrap_or(Value::Null);
    }

    json!({
        "name": project_id,
        "slug": project_id,
        "visibility": "private",
        "metadata": metadata
    })
}

fn repository_payload(repo: &ProjectRepoEntry) -> Value {
    let identity = repo_identity(&repo.id, repo.remote.as_deref());
    json!({
        "provider": identity.provider,
        "owner": identity.owner,
        "name": identity.name,
        "full_name": identity.full_name,
        "default_branch": repo.base_branch,
        "remote_url": repo.remote,
        "metadata": {
            "localId": repo.id,
            "path": repo.path,
            "checkoutMode": repo.checkout_mode,
            "includeByDefault": repo.include_by_default
        }
    })
}

fn bundle_payload(bundle: &ChangeGroup) -> Value {
    json!({
        "title": bundle.title,
        "slug": bundle.id,
        "source": "pushed",
        "lifecycle_state": bundle.state.as_deref().unwrap_or("open"),
        "metadata": {
            "schemaVersion": bundle.schema_version,
            "kind": bundle.kind,
            "repoCount": bundle.repos.len(),
            "commitGroupCount": bundle.commit_groups.len(),
            "nodeCount": bundle.nodes.len(),
            "publicationCount": bundle.publications.len(),
            "pushedBy": "knit"
        }
    })
}

struct RepoIdentity {
    provider: &'static str,
    owner: Option<String>,
    name: String,
    full_name: String,
}

fn repo_identity(repo_id: &str, remote: Option<&str>) -> RepoIdentity {
    let Some(remote) = remote else {
        return fallback_repo_identity(repo_id);
    };
    let remote = remote.trim().trim_end_matches(".git");
    let marker = "github.com";
    let Some(index) = remote.find(marker) else {
        return fallback_repo_identity(repo_id);
    };
    let suffix = remote[index + marker.len()..].trim_start_matches([':', '/']);
    let parts = suffix.split('/').collect::<Vec<_>>();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return fallback_repo_identity(repo_id);
    }
    let owner = parts[0].to_string();
    let name = parts[1].to_string();
    RepoIdentity {
        provider: "github",
        owner: Some(owner.clone()),
        full_name: format!("{owner}/{name}"),
        name,
    }
}

fn fallback_repo_identity(repo_id: &str) -> RepoIdentity {
    RepoIdentity {
        provider: "git",
        owner: None,
        name: repo_id.to_string(),
        full_name: repo_id.to_string(),
    }
}

fn remote_listing(global: bool) -> Result<(KnitConfig, BTreeMap<String, String>)> {
    if global {
        let config = load_global_config()?;
        let sources = config
            .remotes
            .keys()
            .map(|name| (name.clone(), "global".to_string()))
            .collect();
        return Ok((config, sources));
    }

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let Some(root) = find_knit_root(&cwd) else {
        let config = load_global_config()?;
        let sources = config
            .remotes
            .keys()
            .map(|name| (name.clone(), "global".to_string()))
            .collect();
        return Ok((config, sources));
    };

    let global_config = load_global_config()?;
    let workspace_config = load_config(&root)?;
    let mut sources = BTreeMap::new();
    for name in global_config.remotes.keys() {
        sources.insert(name.clone(), "global".to_string());
    }
    for name in workspace_config.remotes.keys() {
        sources.insert(name.clone(), "workspace".to_string());
    }

    let effective = crate::store::merge_effective_config(global_config, workspace_config);

    Ok((effective, sources))
}
