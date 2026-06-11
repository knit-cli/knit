//! KnitHub HTTP client: native HTTP request transport, remote/token/config
//! resolution, project-export fetching, and localizing remote bundles onto the
//! local project's repos.

use super::{HttpResponse, RemoteProjectExport};
use crate::checkout::is_in_place;
use crate::git::{branch_exists, current_branch, git_output, is_ancestor, ref_exists, rev_parse};
use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitConfig, KnitProject, KnitRemote, RepoEntry};
use crate::store::{find_knit_root, load_config, load_effective_config, project_path, read_json, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiEnvelope<T> {
    data: T,
}

pub(super) fn workspace_config() -> Result<(PathBuf, KnitConfig)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    Ok((root, config))
}

pub(super) fn effective_workspace_config() -> Result<(PathBuf, KnitConfig)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_effective_config(&root)?;
    Ok((root, config))
}

pub(super) fn resolve_project_id(
    root: &Path,
    config: &KnitConfig,
    name: Option<&str>,
) -> Result<String> {
    let project_id = name
        .map(slugify)
        .or_else(|| config.active_project.clone())
        .context("No project selected. Pass a project name or run `knit init <name>`.")?;
    if !project_path(root, &project_id).exists() {
        bail!("No local Knit project named `{project_id}`.");
    }
    Ok(project_id)
}

pub(super) fn resolve_remote<'a>(config: &'a KnitConfig, name: &str) -> Result<&'a KnitRemote> {
    let remote_name = slugify(name);
    config
        .remotes
        .get(&remote_name)
        .with_context(|| format!("No KnitHub remote named `{remote_name}`. Run `knit remote add {remote_name} <url>` first."))
}

pub(super) fn resolve_token(name: &str, remote: &KnitRemote) -> Result<String> {
    token_from_env(&slugify(name))
        .or_else(|| remote.token.clone())
        .context("No KnitHub token configured. Set KNITHUB_TOKEN, KNIT_REMOTE_<NAME>_TOKEN, or `knit remote token <name> <token>`.")
}

pub(super) fn token_from_env(name: &str) -> Option<String> {
    let env_name = format!(
        "KNIT_REMOTE_{}_TOKEN",
        name.chars()
            .map(|ch| if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            })
            .collect::<String>()
    );
    std::env::var(env_name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("KNITHUB_TOKEN").ok())
        .filter(|value| !value.trim().is_empty())
}

/// Return configured KnitHub sync remotes in priority order. `syncRemotes` is the
/// multi-target form; `syncRemote` remains the legacy single-target form.
pub fn configured_sync_remote_names(config: &KnitConfig) -> Vec<String> {
    let mut names = Vec::new();
    if !config.sync_remotes.is_empty() {
        for name in &config.sync_remotes {
            push_unique_remote_name(&mut names, name);
        }
    }
    if names.is_empty() {
        if let Some(name) = config.sync_remote.as_deref() {
            push_unique_remote_name(&mut names, name);
        }
    }
    if names.is_empty() && config.remotes.contains_key("knithub") {
        names.push("knithub".to_string());
    }
    names
}

pub(super) fn explicit_remote_names(remote_overrides: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    for name in remote_overrides {
        push_unique_remote_name(&mut names, name);
    }
    names
}

pub(super) fn resolve_sync_remote_names(
    config: &KnitConfig,
    remote_overrides: &[String],
) -> Vec<String> {
    if remote_overrides.is_empty() {
        configured_sync_remote_names(config)
    } else {
        explicit_remote_names(remote_overrides)
    }
}

fn push_unique_remote_name(names: &mut Vec<String>, name: &str) {
    let name = slugify(name);
    if !name.is_empty() && !names.contains(&name) {
        names.push(name);
    }
}

/// Resolve the primary KnitHub sync remote, preferring `syncRemotes[0]`, then
/// legacy `syncRemote`, then a remote literally named `knithub`.
pub(super) fn resolve_sync_remote_name(config: &KnitConfig) -> Result<String> {
    configured_sync_remote_names(config).into_iter().next().context(
        "No KnitHub sync remote configured. Run `knit remote add knithub <url>`, `knit config set sync-remote <name>`, or use explicit prune flags instead of --all.",
    )
}

pub(super) fn load_project_if_present(root: &Path, project_id: &str) -> Result<Option<KnitProject>> {
    let path = project_path(root, project_id);
    if path.exists() {
        read_json(&path).map(Some)
    } else {
        Ok(None)
    }
}

pub(super) fn fetch_project_export(
    remote: &KnitRemote,
    token: &str,
    project_identifier: &str,
) -> Result<RemoteProjectExport> {
    let (owner, slug) = split_project_identifier(project_identifier);
    let path = match owner {
        Some(owner) => format!("/projects/{slug}/export?owner={owner}"),
        None => format!("/projects/{slug}/export"),
    };
    request_json(remote, token, "GET", &path, None)
}

/// Split an `owner/slug` clone reference into its parts. A bare identifier (no
/// `/`) resolves by slug alone, preserving the historical behavior used by
/// local project ids. Each segment is slugified so it is URL-safe and matches
/// how KnitHub stores usernames, org slugs, and project slugs.
pub(super) fn split_project_identifier(identifier: &str) -> (Option<String>, String) {
    match identifier.split_once('/') {
        Some((owner, slug)) if !owner.trim().is_empty() && !slug.trim().is_empty() => {
            (Some(slugify(owner)), slugify(slug))
        }
        _ => (None, slugify(identifier)),
    }
}

pub(super) fn decode_bundle_payload(payload: &Value, bundle_slug: &str) -> Result<ChangeGroup> {
    serde_json::from_value(payload.clone()).with_context(|| {
        format!("Remote bundle `{bundle_slug}` does not contain a supported Knit bundle payload.")
    })
}

pub(super) fn localize_bundle(mut bundle: ChangeGroup, project: &KnitProject) -> Result<ChangeGroup> {
    bundle.project_id = Some(project.id.clone());
    for repo in &mut bundle.repos {
        let local = project
            .repos
            .iter()
            .find(|project_repo| project_repo.id == repo.id)
            .or_else(|| {
                project.repos.iter().find(|project_repo| {
                    project_repo.remote.is_some()
                        && project_repo.remote.as_deref() == repo.remote.as_deref()
                })
            })
            .with_context(|| {
                format!(
                    "{}: remote bundle references a repo that is not in local project `{}`.",
                    repo.id, project.id
                )
            })?;
        repo.path = local.path.clone();
        repo.remote = local.remote.clone().or_else(|| repo.remote.clone());
        repo.base_branch = local.base_branch.clone();
        repo.checkout_mode = local.checkout_mode;
        repo.worktree_path = None;
    }
    Ok(bundle)
}

pub(super) fn prepare_feature_branches(bundle: &ChangeGroup) -> Result<()> {
    for repo in &bundle.repos {
        let Some(branch) = repo.feature_branch.as_deref() else {
            continue;
        };
        let repo_path = PathBuf::from(&repo.path);
        if git_output(&repo_path, ["remote", "get-url", "origin"]).is_err() {
            continue;
        }

        git_output(&repo_path, ["fetch", "origin", branch])
            .with_context(|| format!("{}: failed to fetch origin/{branch}", repo.id))?;
        let remote_ref = format!("origin/{branch}");
        if !ref_exists(&repo_path, &remote_ref) {
            bail!("{}: fetched branch {remote_ref} was not found.", repo.id);
        }
        if !branch_exists(&repo_path, branch) {
            git_output(
                &repo_path,
                [
                    OsString::from("branch"),
                    OsString::from("--track"),
                    OsString::from(branch),
                    OsString::from(&remote_ref),
                ],
            )
            .with_context(|| format!("{}: failed to create local branch {branch}", repo.id))?;
        } else {
            let _ = git_output(
                &repo_path,
                [
                    OsString::from("branch"),
                    OsString::from("--set-upstream-to"),
                    OsString::from(&remote_ref),
                    OsString::from(branch),
                ],
            );
        }
    }
    Ok(())
}

pub(super) fn ensure_remote_bundle_fast_forward(
    local: &ChangeGroup,
    remote: &ChangeGroup,
) -> Result<()> {
    for remote_repo in &remote.repos {
        let Some(remote_head) = remote_repo.head_sha.as_deref() else {
            continue;
        };
        let Some(local_repo) = local.repos.iter().find(|repo| repo.id == remote_repo.id) else {
            continue;
        };
        let Some(local_head) = local_repo.head_sha.as_deref() else {
            continue;
        };
        if local_head == remote_head {
            continue;
        }
        let repo_path = PathBuf::from(&remote_repo.path);
        if !is_ancestor(&repo_path, local_head, remote_head) {
            bail!(
                "{}: remote bundle head {} is not a fast-forward from local head {}. Push or reconcile local work before remote pull.",
                remote_repo.id,
                &remote_head[..remote_head.len().min(12)],
                &local_head[..local_head.len().min(12)]
            );
        }
    }
    Ok(())
}

pub(super) fn fast_forward_feature_checkouts(active: &mut ActiveBundle) -> Result<()> {
    let root = active.root.clone();
    let jobs: Vec<(usize, String, PathBuf, String)> = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .filter_map(|(repo_index, repo)| {
            let branch = repo.feature_branch.as_deref()?;
            let checkout = remote_checkout_dir(&root, repo)?;
            Some((repo_index, repo.id.clone(), checkout, branch.to_string()))
        })
        .collect();

    if jobs.is_empty() {
        return Ok(());
    }

    let results: Vec<(String, Result<(usize, String)>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|(repo_index, repo_id, checkout, branch)| {
                let repo_index = *repo_index;
                let repo_id = repo_id.clone();
                let checkout = checkout.clone();
                let branch = branch.clone();
                scope.spawn(move || {
                    (
                        repo_id.clone(),
                        fast_forward_one_checkout(repo_index, &repo_id, &checkout, &branch),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("fast-forward worker thread panicked"))
            .collect()
    });

    let mut failures = Vec::new();
    for (repo_id, result) in results {
        match result {
            Ok((repo_index, head_sha)) => {
                active.bundle.repos[repo_index].head_sha = Some(head_sha);
            }
            Err(error) => failures.push(format!("{repo_id}: {error:#}")),
        }
    }

    if !failures.is_empty() {
        bail!("fast-forward failed:\n{}", failures.join("\n"));
    }

    active.bundle.updated_at = now_iso();
    Ok(())
}

fn fast_forward_one_checkout(
    repo_index: usize,
    repo_id: &str,
    checkout: &Path,
    branch: &str,
) -> Result<(usize, String)> {
    let actual = current_branch(checkout)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != branch {
        bail!(
            "{repo_id}: expected feature checkout branch `{branch}`, found `{actual}` in {}.",
            checkout.display()
        );
    }
    let remote_ref = format!("origin/{branch}");
    if ref_exists(checkout, &remote_ref) {
        git_output(checkout, ["merge", "--ff-only", &remote_ref])
            .with_context(|| format!("{repo_id}: failed to fast-forward {branch}"))?;
    }
    let head_sha = rev_parse(checkout, "HEAD")
        .with_context(|| format!("{repo_id}: failed to read feature checkout HEAD"))?;
    Ok((repo_index, head_sha))
}

fn remote_checkout_dir(root: &Path, repo: &RepoEntry) -> Option<PathBuf> {
    if let Some(path) = &repo.worktree_path {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        return path.exists().then_some(path);
    }
    if is_in_place(repo) {
        let path = PathBuf::from(&repo.path);
        return path.exists().then_some(path);
    }
    None
}

pub(super) fn request_json<T: DeserializeOwned>(
    remote: &KnitRemote,
    token: &str,
    method: &str,
    path: &str,
    payload: Option<&Value>,
) -> Result<T> {
    decode_response(request(remote, token, method, path, payload)?)
}

pub(super) fn decode_response<T: DeserializeOwned>(response: HttpResponse) -> Result<T> {
    if !(200..300).contains(&response.status) {
        bail!(
            "KnitHub remote returned HTTP {}: {}",
            response.status,
            response.body.trim()
        );
    }
    let envelope: ApiEnvelope<T> =
        serde_json::from_str(&response.body).context("failed to parse KnitHub response")?;
    Ok(envelope.data)
}

pub(super) fn request(
    remote: &KnitRemote,
    token: &str,
    method: &str,
    path: &str,
    payload: Option<&Value>,
) -> Result<HttpResponse> {
    let url = format!("{}{}", api_base_url(&remote.url), path);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build();
    let request = agent
        .request(method, &url)
        .set("content-type", "application/json")
        .set("authorization", &format!("Bearer {token}"));
    let result = match payload {
        Some(value) => {
            let body =
                serde_json::to_string(value).context("failed to serialize request body")?;
            request.send_string(&body)
        }
        None => request.call(),
    };
    let response = match result {
        Ok(response) => response,
        // Non-2xx responses still carry the API's error envelope; surface them
        // as an HttpResponse so callers keep their status-based error paths.
        Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(transport)) => {
            bail!("KnitHub request failed for {url}: {transport}")
        }
    };
    let status = response.status();
    let body = response
        .into_string()
        .context("failed to read KnitHub response body")?;
    Ok(HttpResponse { status, body })
}

pub(super) fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn api_base_url(url: &str) -> String {
    let url = normalize_base_url(url);
    if url.ends_with("/api/v1") {
        url
    } else {
        format!("{url}/api/v1")
    }
}

#[cfg(test)]
mod tests {
    use super::split_project_identifier;

    #[test]
    fn splits_owner_and_slug() {
        assert_eq!(
            split_project_identifier("marc/knit-tools"),
            (Some("marc".to_string()), "knit-tools".to_string())
        );
    }

    #[test]
    fn bare_slug_has_no_owner() {
        assert_eq!(
            split_project_identifier("knit-tools"),
            (None, "knit-tools".to_string())
        );
    }

    #[test]
    fn slugifies_each_segment() {
        assert_eq!(
            split_project_identifier("Ada Lovelace/Knit Tools"),
            (Some("ada-lovelace".to_string()), "knit-tools".to_string())
        );
    }

    #[test]
    fn empty_side_falls_back_to_bare_slug() {
        // A leading or trailing slash is not a valid owner/slug pair; treat the
        // whole thing as a bare identifier and slugify it.
        assert_eq!(
            split_project_identifier("/knit-tools"),
            (None, "knit-tools".to_string())
        );
        assert_eq!(
            split_project_identifier("marc/"),
            (None, "marc".to_string())
        );
    }
}
