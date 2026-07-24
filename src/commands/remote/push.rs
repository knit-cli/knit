//! Remote configuration commands (add/list/show/remove/token) and pushing the
//! local project and bundle artifact to a sync remote, including the implicit
//! sync-on-push.

use super::client::{
    configured_sync_remote_names, decode_response, effective_workspace_config,
    load_project_if_present, normalize_base_url, request, request_json, resolve_project_id,
    resolve_remote, resolve_sync_remote_names, resolve_token, token_from_env, workspace_config,
};
use super::{RemoteArtifact, RemoteBundle, RemoteProject};
use crate::commands::push::PushForce;
use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitConfig, KnitProject, KnitRemote, ProjectRepoEntry};
use crate::output as out;
use crate::store::{
    find_knit_root, load_active_bundle, load_config, load_effective_config, load_global_config,
    save_config, save_global_config, ActiveBundle,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::Path;

const MAX_STDIN_TOKEN_BYTES: u64 = 64 * 1024;

pub fn add_remote(
    name: &str,
    url: &str,
    token: Option<&str>,
    token_stdin: bool,
    global: bool,
) -> Result<()> {
    if token_stdin && !global {
        bail!("--token-stdin requires --global so a user credential is never stored in workspace config");
    }
    let stdin_token = token_stdin.then(read_stdin_token).transpose()?;
    let token = token.or(stdin_token.as_deref());
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
        out::repo(&remote_name)
    );
    if !global && token.is_some() {
        warn_workspace_scoped_token(&remote_name);
    }
    Ok(())
}

fn read_stdin_token() -> Result<String> {
    let mut bytes = Vec::new();
    io::stdin()
        .take(MAX_STDIN_TOKEN_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to read remote token from stdin")?;
    if bytes.len() as u64 > MAX_STDIN_TOKEN_BYTES {
        bail!("remote token exceeds {MAX_STDIN_TOKEN_BYTES} bytes");
    }
    let token = String::from_utf8(bytes).context("remote token from stdin is not UTF-8")?;
    let token = token.trim_end_matches(['\r', '\n']);
    if token.is_empty() {
        bail!("remote token from stdin is empty");
    }
    Ok(token.to_string())
}

/// Workspace config can end up shared (committed, templated, copied between
/// collaborators); tokens belong in per-user storage instead.
fn warn_workspace_scoped_token(remote_name: &str) {
    println!(
        "{} token stored in workspace .knit/config.json. Prefer `--global` or the KNIT_REMOTE_{}_TOKEN environment variable so credentials never live in files a collaborator might receive.",
        out::warn("warning:"),
        remote_name
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            })
            .collect::<String>()
    );
}

pub fn list_remotes(global: bool) -> Result<()> {
    let (config, sources) = remote_listing(global)?;
    if config.remotes.is_empty() {
        println!("{}", out::muted("No remotes configured."));
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
        .with_context(|| format!("No remote named `{remote_name}`."))?;
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

pub fn remote_auth_status(name: &str, json_output: bool) -> Result<()> {
    // Environment-bound credentials live only in the private user config;
    // workspace config is repository-controlled and must not redirect token
    // introspection to another server.
    let config = load_global_config()?;
    let remote_name = slugify(name);
    let remote = resolve_remote(&config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;
    let mut status: Value = request_json(remote, &token, "GET", "/me/access-token", None)?;
    let forge_response = request(remote, &token, "GET", "/me/forge-credentials", None)?;
    let forge_credentials: Value = if (200..300).contains(&forge_response.status) {
        decode_response(forge_response)?
    } else if status
        .get("scopes")
        .and_then(Value::as_array)
        .is_some_and(|scopes| scopes.iter().any(|scope| scope == "forge:credential"))
    {
        bail!(
            "Sync remote returned HTTP {} while checking forge credential capabilities: {}",
            forge_response.status,
            forge_response.body.trim()
        );
    } else {
        Value::Array(Vec::new())
    };
    if let Some(status) = status.as_object_mut() {
        status.insert("forgeCredentials".to_string(), forge_credentials);
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    let kind = status
        .get("tokenKind")
        .and_then(Value::as_str)
        .unwrap_or("legacy");
    let subject = status
        .get("subjectUserId")
        .and_then(Value::as_str)
        .unwrap_or("unbound");
    let environment = status
        .get("environmentId")
        .and_then(Value::as_str)
        .unwrap_or("unbound");
    let expiry = status
        .get("expiresAt")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let scopes = status
        .get("scopes")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    println!("{} {}", out::heading("Remote:"), out::repo(&remote_name));
    println!("{} {kind}", out::heading("Token kind:"));
    println!("{} {subject}", out::heading("Subject:"));
    println!("{} {environment}", out::heading("Environment:"));
    println!("{} {expiry}", out::heading("Expires:"));
    println!("{} {scopes}", out::heading("Scopes:"));
    Ok(())
}

pub fn remove_remote(name: &str, global: bool, revoke: bool) -> Result<()> {
    let (root, mut config) = if global {
        (None, load_global_config()?)
    } else {
        let (root, config) = workspace_config()?;
        (Some(root), config)
    };
    let remote_name = slugify(name);
    let revoke_error = if revoke {
        config
            .remotes
            .get(&remote_name)
            .map(|remote| revoke_remote_token(&remote_name, remote))
            .transpose()
            .err()
    } else {
        None
    };
    if config.remotes.remove(&remote_name).is_none() {
        bail!("No remote named `{remote_name}`.");
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
    if let Some(error) = revoke_error {
        bail!("remote removed locally, but its token could not be revoked: {error:#}");
    }
    Ok(())
}

fn revoke_remote_token(name: &str, remote: &KnitRemote) -> Result<()> {
    let token = resolve_token(name, remote)?;
    let status: Value = request_json(remote, &token, "GET", "/me/access-token", None)?;
    let token_id = status
        .get("id")
        .and_then(Value::as_str)
        .context("remote token introspection returned no token id")?;
    let response = request(
        remote,
        &token,
        "DELETE",
        &format!("/tokens/{token_id}"),
        None,
    )?;
    if !(200..300).contains(&response.status) {
        bail!(
            "Sync remote returned HTTP {}: {}",
            response.status,
            response.body.trim()
        );
    }
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
        .with_context(|| format!("No remote named `{remote_name}`."))?;

    if clear {
        remote.token = None;
        println!("{} {}", out::movement("cleared"), out::repo(&remote_name));
    } else {
        let token = token.context("Pass a token value or use --clear.")?;
        remote.token = Some(token.to_string());
        println!("{} {}", out::movement("stored"), out::repo(&remote_name));
        if !global {
            warn_workspace_scoped_token(&remote_name);
        }
    }

    if let Some(root) = root {
        save_config(&root, &config)?;
    } else {
        save_global_config(&config)?;
    }
    Ok(())
}

pub fn push_project_to_remote(
    name: Option<&str>,
    remote_name: Option<&str>,
    prune: bool,
) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let project_id = resolve_project_id(&root, &config, name)?;
    let remote_names = match remote_name {
        Some(remote_name) => vec![slugify(remote_name)],
        None => configured_sync_remote_names(&config),
    };
    if remote_names.is_empty() {
        bail!("No sync remote configured. Run `knit remote add <name> <url>` first.");
    }
    let project = load_project_if_present(&root, &project_id)?;
    let multiple = remote_names.len() > 1;
    let mut failures = Vec::new();
    for remote_name in &remote_names {
        if multiple {
            println!("{} {}", out::heading("Remote:"), out::repo(remote_name));
        }
        if let Err(error) = push_project_to_one_remote(
            &config,
            remote_name,
            &root,
            &project_id,
            project.as_ref(),
            prune,
        ) {
            println!(
                "{} {error:#}",
                out::warn(format!("push failed ({remote_name}):"))
            );
            failures.push(remote_name.clone());
        }
    }
    if failures.len() == remote_names.len() {
        bail!(
            "project push failed for every remote: {}",
            failures.join(", ")
        );
    }
    Ok(())
}

fn push_project_to_one_remote(
    config: &KnitConfig,
    remote_name: &str,
    root: &Path,
    project_id: &str,
    project: Option<&KnitProject>,
    prune: bool,
) -> Result<()> {
    let remote = resolve_remote(config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let pushed = upsert_project(remote, &token, project_id, project)?;
    let repo_count = match project {
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

    // After the upsert, drop remote repo records the local shape no longer has.
    if prune {
        let keep_ids: Vec<String> = project
            .map(|project| project.repos.iter().map(|repo| repo.id.clone()).collect())
            .unwrap_or_default();
        prune_remote_repositories(remote, &token, &pushed.slug, &keep_ids)?;
    }

    // Best-effort: also upload the user's saved views for this project. A server
    // without the views endpoint must not fail the project push.
    if let Err(error) = upload_views(remote, &token, root, &pushed.slug) {
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

/// Push the current user's saved views for a project to the sync remote.
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

/// Push the project architecture artifact (repo-level rollup produced by
/// `urdir kg architecture`) to a sync remote. Reads the conventional
/// `.urdir/kg/<slug>/architecture.json`; a missing file is a soft skip with a
/// hint, not an error, so `knit sync push` (everything) stays safe.
pub fn push_architecture_to_remote(name: Option<&str>, remote_name: &str) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let project_id = resolve_project_id(&root, &config, name)?;
    let path = root
        .join(".urdir")
        .join("kg")
        .join(&project_id)
        .join("architecture.json");
    let raw = match read_graph_artifact(
        &path,
        "architecture",
        "urdir kg architecture --project <slug>",
    )? {
        Some(v) => v,
        None => return Ok(()),
    };
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    request_json::<Value>(
        remote,
        &token,
        "PUT",
        &format!("/projects/{project_id}/architecture"),
        Some(&raw),
    )?;
    let repos = raw
        .get("repos")
        .and_then(|v| v.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    let edges = raw
        .get("edges")
        .and_then(|v| v.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    println!(
        "{} {} {}",
        out::movement("pushed architecture"),
        out::repo(&project_id),
        out::muted(format!("{repos} repo(s), {edges} edge(s)"))
    );
    Ok(())
}

/// Push the knowledge-graph viz slice (produced by `urdir kg viz`) to a
/// sync remote. Reads `.urdir/kg/<slug>/viz.json`; missing file is a soft
/// skip with a hint.
pub fn push_kg_graph_to_remote(name: Option<&str>, remote_name: &str) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let project_id = resolve_project_id(&root, &config, name)?;
    let path = root
        .join(".urdir")
        .join("kg")
        .join(&project_id)
        .join("viz.json");
    let raw = match read_graph_artifact(&path, "knowledge graph", "urdir kg viz --project <slug>")?
    {
        Some(v) => v,
        None => return Ok(()),
    };
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    request_json::<Value>(
        remote,
        &token,
        "PUT",
        &format!("/projects/{project_id}/kg-graph"),
        Some(&raw),
    )?;
    let nodes = raw.get("nodeCount").and_then(|v| v.as_u64()).unwrap_or(0);
    let edges = raw.get("edgeCount").and_then(|v| v.as_u64()).unwrap_or(0);
    let truncated = raw
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    println!(
        "{} {} {}",
        out::movement("pushed kg graph"),
        out::repo(&project_id),
        out::muted(format!(
            "{nodes} node(s), {edges} edge(s){}",
            if truncated { " (truncated)" } else { "" }
        ))
    );
    Ok(())
}

/// Load a graph artifact JSON, with a soft-skip note when it is absent (so a
/// bare `knit sync push` does not hard-fail on workspaces that have not run the
/// producing urdir command). Returns `Ok(None)` after printing a hint.
fn read_graph_artifact(path: &Path, label: &str, hint: &str) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text).with_context(|| {
            format!("failed to parse {label} artifact at {}", path.display())
        })?)),
        Err(_) => {
            println!(
                "{}",
                out::muted(format!(
                    "no {label} artifact at {}; skipped (run `{hint}` to produce one)",
                    path.display()
                ))
            );
            Ok(None)
        }
    }
}

pub fn push_bundle_to_remote(
    remote_name: &str,
    project: Option<&str>,
    force: PushForce,
) -> Result<()> {
    let active = load_active_bundle()?;
    push_active_bundle_to_remote(remote_name, project, &active, force)
}

fn push_active_bundle_to_remote(
    remote_name: &str,
    project: Option<&str>,
    active: &ActiveBundle,
    force: PushForce,
) -> Result<()> {
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
    let artifact = push_bundle_artifact(remote, &token, &pushed_bundle.id, &active.bundle, force)?;
    let history_result = super::history::push_project_history_events(
        remote,
        &token,
        &pushed_project.slug,
        &active.root,
        &project_id,
    );

    println!(
        "{} {} -> {}",
        out::movement(if force.is_force() {
            "pushed (forced)"
        } else {
            "pushed"
        }),
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

/// Push the resolved bundle artifact to configured sync remote(s) alongside a
/// git push, when enabled.
///
/// Resolution order for remotes: repeated explicit `--remote`, then
/// `syncRemotes`, then legacy `sync_remote`, then the sole configured remote.
/// With no remote configured this is a silent no-op. The `push_sync` config
/// disables implicit sync, but explicit `--remote` still forces it. `--no-remote`
/// always skips. Sync failures are reported as warnings and never fail the git
/// push that already succeeded. A forced git push (`knit push --force*`)
/// carries the same force mode into this artifact sync so branches and ledger
/// move together.
pub fn maybe_sync_bundle_to_remote(
    remote_overrides: &[String],
    no_remote: bool,
    force: PushForce,
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
                out::warn(format!("remote sync skipped ({remote_name}):"))
            );
            continue;
        }
        if multiple {
            println!("{} {}", out::heading("Remote:"), out::repo(&remote_name));
        }
        if let Err(error) = push_bundle_to_remote(&remote_name, None, force) {
            println!(
                "{} {error:#}",
                out::warn(format!("remote sync skipped ({remote_name}):"))
            );
        }
    }
    Ok(())
}

/// Push the resolved bundle artifact to the sync remotes when push-sync is enabled.
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

pub fn sync_active_bundle_to_remote_if_enabled(
    active: &ActiveBundle,
    remote_overrides: &[String],
    no_remote: bool,
) -> Result<()> {
    if no_remote {
        return Ok(());
    }
    let config = load_effective_config(&active.root)?;
    if !config.push_sync && remote_overrides.is_empty() {
        return Ok(());
    }
    let remote_names = resolve_sync_remote_names(&config, remote_overrides);
    if remote_names.is_empty() {
        return Ok(());
    }

    sync_active_bundle_to_remote_names(&config, &remote_names, active)
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
        if let Err(error) = push_bundle_to_remote(remote_name, None, PushForce::No) {
            failures.push(format!("{remote_name}: {error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "Sync failed for {} remote(s):\n{}",
            failures.len(),
            failures.join("\n")
        )
    }
}

fn sync_active_bundle_to_remote_names(
    config: &KnitConfig,
    remote_names: &[String],
    active: &ActiveBundle,
) -> Result<()> {
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
        if let Err(error) = push_active_bundle_to_remote(remote_name, None, active, PushForce::No) {
            failures.push(format!("{remote_name}: {error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "Sync failed for {} remote(s):\n{}",
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

/// A repository record as the sync remote lists it. `local_id` is the id the
/// local project shape uses; the server may carry it top-level, in metadata, or
/// fall back to the repo name (mirroring its own `local_repo_id`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteRepositoryRecord {
    id: String,
    #[serde(default)]
    local_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    metadata: Value,
}

impl RemoteRepositoryRecord {
    fn local_id(&self) -> Option<String> {
        self.local_id
            .clone()
            .or_else(|| {
                self.metadata
                    .get("localId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| self.name.clone())
    }
}

/// Delete remote repository records whose local id is not in `keep_ids`, so the
/// remote repo set converges on the local project shape. Per-repo delete
/// failures are collected and reported; the sweep fails only if any delete did.
fn prune_remote_repositories(
    remote: &KnitRemote,
    token: &str,
    project_slug: &str,
    keep_ids: &[String],
) -> Result<()> {
    let records: Vec<RemoteRepositoryRecord> = request_json(
        remote,
        token,
        "GET",
        &format!("/projects/{project_slug}/repositories"),
        None,
    )?;

    let mut failures = Vec::new();
    for record in &records {
        let local_id = record.local_id();
        if local_id
            .as_deref()
            .is_some_and(|id| keep_ids.iter().any(|keep| keep == id))
        {
            continue;
        }
        let label = local_id.as_deref().unwrap_or(&record.id);
        match request(
            remote,
            token,
            "DELETE",
            &format!("/projects/{project_slug}/repositories/{}", record.id),
            None,
        ) {
            Ok(response) if (200..300).contains(&response.status) || response.status == 404 => {
                println!(
                    "{} {} {}",
                    out::movement("pruned"),
                    out::repo(label),
                    out::muted(&record.id)
                );
            }
            Ok(response) => failures.push(format!(
                "{label}: HTTP {} {}",
                response.status,
                response.body.trim()
            )),
            Err(error) => failures.push(format!("{label}: {error:#}")),
        }
    }

    if !failures.is_empty() {
        bail!(
            "failed to prune remote repositories:\n{}",
            failures.join("\n")
        );
    }
    Ok(())
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

enum ArtifactPushOutcome {
    Pushed(RemoteArtifact),
    RemoteAhead,
    /// A forced-with-lease push was refused: the remote's current artifact no
    /// longer matches the hash fetched for the lease (someone pushed
    /// meanwhile). Carries the hash the remote reported as current, when it
    /// sent one.
    LeaseMismatch {
        current: Option<String>,
    },
}

/// Add the artifact-plane force fields to a POST body, mirroring the sync
/// remote's contract: `force: true` alone is an unconditional replace;
/// `force: true` plus `expectedArtifactHash` is a compare-and-swap against the
/// remote's current artifact. A lease force with no known remote artifact
/// sends a plain body: there is nothing to overwrite, so the normal
/// fast-forward check is the safest gate for that first push.
fn apply_artifact_force_fields(payload: &mut Value, force: PushForce, lease: Option<&str>) {
    let Some(fields) = payload.as_object_mut() else {
        return;
    };
    match force {
        PushForce::No => {}
        PushForce::Unconditional => {
            fields.insert("force".to_string(), Value::Bool(true));
        }
        PushForce::WithLease => {
            if let Some(lease) = lease {
                fields.insert("force".to_string(), Value::Bool(true));
                fields.insert(
                    "expectedArtifactHash".to_string(),
                    Value::String(lease.to_string()),
                );
            }
        }
    }
}

/// Fetch the hash of the remote bundle's current artifact for a force lease.
/// Uses the per-bundle artifact index, which the server returns newest-first;
/// the newest record is the current artifact. `None` means the bundle has no
/// artifact yet.
fn fetch_current_artifact_hash(
    remote: &KnitRemote,
    token: &str,
    bundle_id: &str,
) -> Result<Option<String>> {
    let artifacts: Vec<RemoteArtifact> = request_json(
        remote,
        token,
        "GET",
        &format!("/bundles/{bundle_id}/artifacts"),
        None,
    )?;
    Ok(artifacts
        .into_iter()
        .next()
        .map(|artifact| artifact.artifact_hash))
}

/// The error envelope a sync remote sends with a refused artifact push. Only
/// the `kind` and lease details matter here; unknown shapes decode to `None`s
/// and fall back to the plain fast-forward interpretation.
#[derive(Debug, serde::Deserialize)]
struct RemotePushErrorEnvelope {
    #[serde(default)]
    error: Option<RemotePushError>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemotePushError {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    current_artifact_hash: Option<String>,
}

fn push_bundle_artifact_outcome(
    remote: &KnitRemote,
    token: &str,
    bundle_id: &str,
    bundle: &ChangeGroup,
    force: PushForce,
) -> Result<ArtifactPushOutcome> {
    let lease = if force.wants_lease() {
        fetch_current_artifact_hash(remote, token, bundle_id)?
    } else {
        None
    };
    let mut payload = json!({
        "kind": "bundle",
        "schema_version": bundle.schema_version,
        "producer": "knit",
        "producer_version": env!("CARGO_PKG_VERSION"),
        "payload": bundle
    });
    apply_artifact_force_fields(&mut payload, force, lease.as_deref());
    let response = request(
        remote,
        token,
        "POST",
        &format!("/bundles/{bundle_id}/artifacts"),
        Some(&payload),
    )?;
    // The remote refuses any artifact whose ledger would drop nodes the
    // current remote artifact records: another user (or another machine)
    // pushed work this workspace has not seen yet. Under a force lease the
    // same status instead means the remote artifact changed since the lease
    // hash was fetched.
    if response.status == 409 {
        let envelope: RemotePushErrorEnvelope =
            serde_json::from_str(&response.body).unwrap_or(RemotePushErrorEnvelope { error: None });
        if let Some(error) = envelope.error {
            if error.kind.as_deref() == Some("leaseMismatch") {
                return Ok(ArtifactPushOutcome::LeaseMismatch {
                    current: error.current_artifact_hash,
                });
            }
        }
        return Ok(ArtifactPushOutcome::RemoteAhead);
    }
    decode_response(response).map(ArtifactPushOutcome::Pushed)
}

/// The per-bundle failure message for a refused force lease. Kept in one place
/// so the single push and the sweep report the same thing.
fn lease_mismatch_message(bundle_id: &str, current: Option<&str>) -> String {
    let current = current
        .map(|hash| format!(" (remote artifact is now {hash})"))
        .unwrap_or_default();
    format!(
        "{bundle_id}: remote artifact changed since fetch{current}. Run `knit sync pull --bundles` to see the new state, then force-push again."
    )
}

fn push_bundle_artifact(
    remote: &KnitRemote,
    token: &str,
    bundle_id: &str,
    bundle: &ChangeGroup,
    force: PushForce,
) -> Result<RemoteArtifact> {
    match push_bundle_artifact_outcome(remote, token, bundle_id, bundle, force)? {
        ArtifactPushOutcome::Pushed(artifact) => Ok(artifact),
        ArtifactPushOutcome::RemoteAhead => bail!(
            "{}: the remote has recorded bundle work this workspace does not include. Run `knit pull` to fast-forward (or `knit pull --merge` if the ledgers diverged), then push again, or overwrite the remote ledger with `knit sync push --bundles --force-with-lease`.",
            bundle.id
        ),
        ArtifactPushOutcome::LeaseMismatch { current } => {
            bail!("{}", lease_mismatch_message(&bundle.id, current.as_deref()))
        }
    }
}

/// Push every local bundle artifact — open, landed, and archived alike — to a
/// sync remote so the remote's lifecycle state converges on the local
/// ledger. Bundles whose remote ledger is ahead of this workspace are skipped
/// with a warning (catching up is `knit sync pull`'s job); other per-bundle
/// failures are collected and fail the sweep at the end.
pub fn push_all_bundles_to_remote(
    remote_name: &str,
    project: Option<&str>,
    exclude_bundle: Option<&str>,
    force: PushForce,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_effective_config(&root)?;
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;

    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        return Ok(());
    }
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(std::ffi::OsStr::new("json")))
        .collect();
    paths.sort();

    let mut project_slugs: BTreeMap<String, String> = BTreeMap::new();
    let mut pushed = 0usize;
    let mut remote_ahead = Vec::new();
    let mut failures = Vec::new();

    for path in paths {
        let bundle: ChangeGroup = match crate::store::read_json(&path) {
            Ok(bundle) => bundle,
            Err(error) => {
                failures.push(format!("{}: {error:#}", path.display()));
                continue;
            }
        };
        if exclude_bundle == Some(bundle.id.as_str()) {
            continue;
        }
        let Some(project_id) = bundle
            .project_id
            .clone()
            .or_else(|| project.map(slugify))
            .or_else(|| config.active_project.clone())
        else {
            failures.push(format!("{}: no project recorded on the bundle", bundle.id));
            continue;
        };
        let project_slug = match project_slugs.get(&project_id) {
            Some(slug) => slug.clone(),
            None => {
                let local_project = load_project_if_present(&root, &project_id)?;
                let upserted = upsert_project(remote, &token, &project_id, local_project.as_ref())?;
                if let Some(local) = local_project.as_ref() {
                    push_repositories(remote, &token, &upserted.slug, &local.repos)?;
                }
                project_slugs.insert(project_id.clone(), upserted.slug.clone());
                upserted.slug
            }
        };
        let outcome =
            upsert_bundle(remote, &token, &project_slug, &bundle).and_then(|remote_bundle| {
                push_bundle_artifact_outcome(remote, &token, &remote_bundle.id, &bundle, force)
            });
        match outcome {
            Ok(ArtifactPushOutcome::Pushed(_)) => {
                pushed += 1;
                if force.is_force() {
                    println!(
                        "{} {}",
                        out::movement("pushed (forced)"),
                        out::repo(&bundle.id)
                    );
                }
            }
            Ok(ArtifactPushOutcome::RemoteAhead) => remote_ahead.push(bundle.id.clone()),
            Ok(ArtifactPushOutcome::LeaseMismatch { current }) => {
                failures.push(lease_mismatch_message(&bundle.id, current.as_deref()));
            }
            Err(error) => failures.push(format!("{}: {error:#}", bundle.id)),
        }
    }

    println!("{} {pushed} bundle artifact(s)", out::movement("pushed"));
    if !remote_ahead.is_empty() {
        println!(
            "{} {}: the remote ledger is ahead; run `knit sync pull --bundles` to fast-forward, then push again, or overwrite the remote ledger with `knit sync push --bundles --force-with-lease`",
            out::warn("Skipped"),
            remote_ahead.join(", ")
        );
    }
    if !failures.is_empty() {
        bail!(
            "failed to push {} bundle artifact(s):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    Ok(())
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
        // The shared project record must not carry this machine's filesystem
        // layout: every collaborator pushes the same project, and pulls
        // rebuild paths from their own clones.
        let mut shared = project.clone();
        for repo in &mut shared.repos {
            repo.path = String::new();
        }
        metadata["knitProject"] = serde_json::to_value(&shared).unwrap_or(Value::Null);
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
    // No `path` here: repository records are shared across collaborators and
    // a machine-local checkout path would just be whoever pushed last.
    json!({
        "provider": identity.provider,
        "owner": identity.owner,
        "name": identity.name,
        "full_name": identity.full_name,
        "default_branch": repo.base_branch,
        "remote_url": repo.remote,
        "metadata": {
            "localId": repo.id,
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
        "lifecycle_state": bundle.state.unwrap_or(crate::model::BundleState::Open).as_str(),
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

#[cfg(test)]
mod tests {
    use super::{apply_artifact_force_fields, lease_mismatch_message};
    use crate::commands::push::PushForce;
    use serde_json::json;

    fn base_payload() -> serde_json::Value {
        json!({"kind": "bundle", "payload": {}})
    }

    #[test]
    fn no_force_leaves_the_payload_untouched() {
        let mut payload = base_payload();
        apply_artifact_force_fields(&mut payload, PushForce::No, None);
        assert!(payload.get("force").is_none());
        assert!(payload.get("expectedArtifactHash").is_none());
    }

    #[test]
    fn unconditional_force_sends_force_without_a_lease() {
        let mut payload = base_payload();
        apply_artifact_force_fields(&mut payload, PushForce::Unconditional, None);
        assert_eq!(payload["force"], json!(true));
        assert!(payload.get("expectedArtifactHash").is_none());
    }

    #[test]
    fn lease_force_sends_force_and_the_fetched_hash() {
        let mut payload = base_payload();
        apply_artifact_force_fields(&mut payload, PushForce::WithLease, Some("hash-1"));
        assert_eq!(payload["force"], json!(true));
        assert_eq!(payload["expectedArtifactHash"], json!("hash-1"));
    }

    #[test]
    fn lease_force_without_a_remote_artifact_sends_a_plain_push() {
        // Nothing to lease against: the plain fast-forward check is the
        // safest gate for what is effectively a first push.
        let mut payload = base_payload();
        apply_artifact_force_fields(&mut payload, PushForce::WithLease, None);
        assert!(payload.get("force").is_none());
        assert!(payload.get("expectedArtifactHash").is_none());
    }

    #[test]
    fn lease_mismatch_message_names_the_bundle_and_current_hash() {
        let message = lease_mismatch_message("feature-a", Some("abc123"));
        assert!(message.contains("feature-a: remote artifact changed since fetch"));
        assert!(message.contains("abc123"));
        let without = lease_mismatch_message("feature-a", None);
        assert!(without.contains("remote artifact changed since fetch"));
    }
}
