//! Remote configuration commands (add/list/show/remove/token) and pushing the
//! local project and bundle artifact to a KnitHub remote, including the implicit
//! sync-on-push.

use super::client::{
    decode_response, load_project_if_present, normalize_base_url, request, request_json,
    resolve_project_id, resolve_remote, resolve_token, token_from_env, workspace_config,
};
use super::{RemoteArtifact, RemoteBundle, RemoteProject};
use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitProject, KnitRemote, ProjectRepoEntry};
use crate::output as out;
use crate::store::{load_active_bundle, load_config, save_config};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub fn add_remote(name: &str, url: &str, token: Option<&str>) -> Result<()> {
    let (root, mut config) = workspace_config()?;
    let remote_name = slugify(name);
    config.remotes.insert(
        remote_name.clone(),
        KnitRemote {
            url: normalize_base_url(url),
            token: token.map(ToString::to_string),
        },
    );
    save_config(&root, &config)?;
    println!("{} {}", out::movement("configured"), out::repo(remote_name));
    Ok(())
}

pub fn list_remotes() -> Result<()> {
    let (_root, config) = workspace_config()?;
    if config.remotes.is_empty() {
        println!("{}", out::muted("No KnitHub remotes configured."));
        return Ok(());
    }

    for (name, remote) in config.remotes {
        let token_label = if token_from_env(&name).is_some() {
            "token from env"
        } else if remote.token.is_some() {
            "stored token"
        } else {
            "no token"
        };
        println!(
            "{} {} {}",
            out::repo(name),
            remote.url,
            out::muted(token_label)
        );
    }
    Ok(())
}

pub fn show_remote(name: &str) -> Result<()> {
    let (_root, config) = workspace_config()?;
    let remote_name = slugify(name);
    let remote = config
        .remotes
        .get(&remote_name)
        .with_context(|| format!("No KnitHub remote named `{remote_name}`."))?;
    println!("{} {}", out::heading("Remote:"), out::repo(&remote_name));
    println!("{} {}", out::heading("URL:"), remote.url);
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

pub fn remove_remote(name: &str) -> Result<()> {
    let (root, mut config) = workspace_config()?;
    let remote_name = slugify(name);
    if config.remotes.remove(&remote_name).is_none() {
        bail!("No KnitHub remote named `{remote_name}`.");
    }
    save_config(&root, &config)?;
    println!("{} {}", out::movement("removed"), out::repo(remote_name));
    Ok(())
}

pub fn set_remote_token(name: &str, token: Option<&str>, clear: bool) -> Result<()> {
    let (root, mut config) = workspace_config()?;
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

    save_config(&root, &config)?;
    Ok(())
}

pub fn push_project_to_remote(name: Option<&str>, remote_name: &str) -> Result<()> {
    let (root, config) = workspace_config()?;
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
    Ok(())
}

pub fn push_bundle_to_remote(remote_name: &str, project: Option<&str>) -> Result<()> {
    let active = load_active_bundle()?;
    let config = load_config(&active.root)?;
    let project_id = project
        .map(slugify)
        .or_else(|| active.bundle.project_id.clone())
        .or_else(|| config.active_project.clone())
        .context("No project selected. Pass --project or run `knit project init <name>`.")?;
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let local_project = load_project_if_present(&active.root, &project_id)?;
    let pushed_project = upsert_project(remote, &token, &project_id, local_project.as_ref())?;
    if let Some(project) = local_project.as_ref() {
        push_repositories(remote, &token, &pushed_project.slug, &project.repos)?;
    }

    let pushed_bundle = upsert_bundle(remote, &token, &pushed_project.slug, &active.bundle)?;
    let artifact = push_bundle_artifact(remote, &token, &pushed_bundle.id, &active.bundle)?;

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
    Ok(())
}

/// Push the resolved bundle artifact to the configured KnitHub remote alongside a
/// git push, when enabled.
///
/// Resolution order for the remote: explicit `--remote`, then `sync_remote`, then a
/// remote literally named `knithub`. With no remote configured this is a silent
/// no-op. The `push_sync` config disables the implicit sync, but an explicit
/// `--remote` still forces it. `--no-remote` always skips. A sync failure is
/// reported as a warning and never fails the git push that already succeeded.
pub fn maybe_sync_bundle_to_remote(remote_override: Option<&str>, no_remote: bool) -> Result<()> {
    if no_remote {
        return Ok(());
    }
    let Ok((_, config)) = workspace_config() else {
        return Ok(());
    };
    if !config.push_sync && remote_override.is_none() {
        return Ok(());
    }
    let Some(remote_name) = remote_override
        .map(slugify)
        .or_else(|| config.sync_remote.clone())
        .or_else(|| {
            config
                .remotes
                .contains_key("knithub")
                .then(|| "knithub".to_string())
        })
    else {
        return Ok(());
    };
    if resolve_remote(&config, &remote_name).is_err() {
        // An explicitly requested remote that does not exist is an error; an
        // implicit fallback that is missing is just skipped.
        if remote_override.is_some() {
            resolve_remote(&config, &remote_name)?;
        }
        return Ok(());
    }

    if let Err(error) = push_bundle_to_remote(&remote_name, None) {
        println!("{} {error:#}", out::warn("KnitHub sync skipped:"));
    }
    Ok(())
}

/// Push the resolved bundle artifact to KnitHub when push-sync is enabled.
///
/// Unlike `maybe_sync_bundle_to_remote`, sync failures are returned to the
/// caller. This is used after landing so a stale remote lifecycle state is
/// visible instead of being hidden behind a best-effort warning.
pub fn sync_bundle_to_remote_if_enabled(
    remote_override: Option<&str>,
    no_remote: bool,
) -> Result<()> {
    if no_remote {
        return Ok(());
    }
    let Ok((_, config)) = workspace_config() else {
        return Ok(());
    };
    if !config.push_sync && remote_override.is_none() {
        return Ok(());
    }
    let Some(remote_name) = remote_override
        .map(slugify)
        .or_else(|| config.sync_remote.clone())
        .or_else(|| {
            config
                .remotes
                .contains_key("knithub")
                .then(|| "knithub".to_string())
        })
    else {
        return Ok(());
    };
    if resolve_remote(&config, &remote_name).is_err() {
        if remote_override.is_some() {
            resolve_remote(&config, &remote_name)?;
        }
        return Ok(());
    }

    push_bundle_to_remote(&remote_name, None)
}

/// Push the resolved bundle artifact to KnitHub, failing if no remote is
/// configured or if the sync cannot complete.
pub fn sync_bundle_to_remote(remote_override: Option<&str>) -> Result<()> {
    let (_, config) = workspace_config()?;
    let remote_name = remote_override
        .map(slugify)
        .or_else(|| config.sync_remote.clone())
        .or_else(|| {
            config
                .remotes
                .contains_key("knithub")
                .then(|| "knithub".to_string())
        })
        .context("No KnitHub remote configured. Run `knit remote add knithub <url>` first.")?;
    resolve_remote(&config, &remote_name)?;
    push_bundle_to_remote(&remote_name, None)
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
