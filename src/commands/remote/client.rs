//! Sync remote HTTP client: native HTTP request transport, remote/token/config
//! resolution, project-export fetching, and localizing remote bundles onto the
//! local project's repos.

use super::{HttpResponse, RemoteBundleDetail, RemoteExportBundle, RemoteProjectExport};
use crate::checkout::is_in_place;
use crate::git::{
    branch_exists, current_branch, git_output, git_output_with_env, is_ancestor, ref_exists,
    rev_parse,
};
use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitConfig, KnitProject, KnitRemote, RepoEntry};
use crate::store::{
    find_knit_root, load_config, load_effective_config, project_path, read_json, ActiveBundle,
};
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

pub(crate) fn resolve_remote<'a>(config: &'a KnitConfig, name: &str) -> Result<&'a KnitRemote> {
    let remote_name = slugify(name);
    config.remotes.get(&remote_name).with_context(|| {
        format!("No remote named `{remote_name}`. Run `knit remote add {remote_name} <url>` first.")
    })
}

pub(crate) fn resolve_token(name: &str, remote: &KnitRemote) -> Result<String> {
    token_from_env(&slugify(name))
        .or_else(|| remote.token.clone())
        .context("No remote token configured. Set KNIT_REMOTE_<NAME>_TOKEN, KNIT_REMOTE_TOKEN, or `knit remote token <name> <token>`.")
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
        .or_else(|| std::env::var("KNIT_REMOTE_TOKEN").ok())
        .filter(|value| !value.trim().is_empty())
}

/// Return the sync remotes in priority order. By default the remotes list
/// itself is the sync set: every configured remote participates, names carry no
/// special meaning. An explicit `syncRemotes` (or legacy `syncRemote`) narrows
/// that set for configs that want some remotes excluded from routine sync.
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
    if names.is_empty() {
        for name in config.remotes.keys() {
            push_unique_remote_name(&mut names, name);
        }
    }
    names
}

/// Run `attempt` against each configured sync remote in priority order and
/// return the first success. An unreachable remote is reported and skipped; the
/// last candidate's error (or an explicit override's error) propagates, so the
/// call only fails when no remote could serve it. Read paths (pull, fetch,
/// history) use this; push paths fan out over every remote instead.
pub(super) fn with_first_available_remote<T>(
    config: &KnitConfig,
    remote_override: Option<&str>,
    attempt: impl Fn(&str, &KnitRemote, &str) -> Result<T>,
) -> Result<T> {
    let candidates: Vec<String> = match remote_override {
        Some(name) => vec![slugify(name)],
        None => configured_sync_remote_names(config),
    };
    if candidates.is_empty() {
        bail!("No sync remote configured. Run `knit remote add <name> <url>` first.");
    }
    let explicit = remote_override.is_some();
    let last = candidates.len() - 1;
    for (index, remote_name) in candidates.iter().enumerate() {
        let result = resolve_remote(config, remote_name)
            .and_then(|remote| Ok((remote, resolve_token(remote_name, remote)?)))
            .and_then(|(remote, token)| attempt(remote_name, remote, &token));
        match result {
            Ok(value) => return Ok(value),
            Err(error) => {
                if explicit || index == last {
                    return Err(error);
                }
                println!(
                    "{} {error:#}",
                    crate::output::warn(format!("remote {remote_name} unavailable, trying next:"))
                );
            }
        }
    }
    unreachable!("candidates checked non-empty above")
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

/// Resolve the primary sync remote: the first name `configured_sync_remote_names`
/// returns. Callers that can address only one remote (per-record deletes,
/// archive-by-id) use this; fan-out callers iterate the full list instead.
pub(super) fn resolve_sync_remote_name(config: &KnitConfig) -> Result<String> {
    configured_sync_remote_names(config).into_iter().next().context(
        "No sync remote configured. Run `knit remote add <name> <url>`, `knit config set sync-remote <name>`, or use explicit prune flags instead of --all.",
    )
}

pub(super) fn load_project_if_present(
    root: &Path,
    project_id: &str,
) -> Result<Option<KnitProject>> {
    let path = project_path(root, project_id);
    if path.exists() {
        read_json(&path).map(Some)
    } else {
        Ok(None)
    }
}

/// Fetch the project export. The request is always for the slim shape: bundle
/// records with artifact metadata but no payloads, and no history events. One
/// response carrying every bundle payload plus the whole history ledger is
/// what a large project cannot serve without exhausting the server's memory;
/// payloads are fetched per bundle (see [`fetch_bundle_artifact`]) and history
/// through its own endpoint. A server that ignores the parameters still
/// answers with the full shape, and callers use whatever it inlined.
pub(super) fn fetch_project_export(
    remote: &KnitRemote,
    token: Option<&str>,
    project_identifier: &str,
) -> Result<RemoteProjectExport> {
    let (owner, slug) = split_project_identifier(project_identifier);
    let path = match owner {
        Some(owner) => {
            format!("/projects/{slug}/export?artifacts=none&history=false&owner={owner}")
        }
        None => format!("/projects/{slug}/export?artifacts=none&history=false"),
    };
    request_json_with_optional_token(remote, token, "GET", &path, None)
}

/// Fetch one bundle's current artifact by remote bundle id, returning the
/// decoded payload and the artifact's hash. Callers must keep these calls
/// sequential: fetching one payload at a time is the whole point of the slim
/// export.
pub(super) fn fetch_bundle_artifact(
    remote: &KnitRemote,
    token: Option<&str>,
    remote_bundle_id: &str,
    bundle_slug: &str,
) -> Result<(ChangeGroup, String)> {
    let detail: RemoteBundleDetail = request_json_with_optional_token(
        remote,
        token,
        "GET",
        &format!("/bundles/{remote_bundle_id}?include=artifact"),
        None,
    )
    .with_context(|| format!("failed to fetch the artifact for remote bundle `{bundle_slug}`"))?;
    let artifact = detail
        .current_artifact
        .with_context(|| format!("Remote bundle `{bundle_slug}` has no current artifact."))?;
    let payload = artifact
        .payload
        .with_context(|| format!("Remote bundle `{bundle_slug}` returned no artifact payload."))?;
    Ok((
        decode_bundle_payload(&payload, bundle_slug)?,
        artifact.artifact_hash,
    ))
}

/// Resolve an export entry's bundle payload: use the payload the server
/// inlined when it ignored the slim request (older deployments), otherwise
/// fetch that one bundle's artifact.
pub(super) fn resolve_export_bundle_payload(
    remote: &KnitRemote,
    token: Option<&str>,
    entry: &RemoteExportBundle,
) -> Result<(ChangeGroup, String)> {
    let artifact = entry
        .current_artifact
        .as_ref()
        .with_context(|| format!("Remote bundle `{}` has no current artifact.", entry.slug))?;
    match artifact.payload.as_ref() {
        Some(payload) => Ok((
            decode_bundle_payload(payload, &entry.slug)?,
            artifact.artifact_hash.clone(),
        )),
        None => fetch_bundle_artifact(remote, token, &entry.id, &entry.slug),
    }
}

/// Split an `owner/slug` clone reference into its parts. A bare identifier (no
/// `/`) resolves by slug alone, preserving the historical behavior used by
/// local project ids. Each segment is slugified so it is URL-safe and matches
/// how the hosted server stores usernames, org slugs, and project slugs.
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

pub(super) fn localize_bundle(
    mut bundle: ChangeGroup,
    project: &KnitProject,
) -> Result<ChangeGroup> {
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

#[derive(Debug)]
pub(super) struct MissingRemoteBranch {
    pub(super) repo_id: String,
    pub(super) branch: String,
    detail: String,
}

impl MissingRemoteBranch {
    pub(super) fn summary(&self) -> String {
        format!("{}: origin has no branch {}", self.repo_id, self.branch)
    }
}

#[derive(Debug)]
pub(super) struct MissingRemoteBranches(pub(super) Vec<MissingRemoteBranch>);

impl std::fmt::Display for MissingRemoteBranches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let details = self
            .0
            .iter()
            .map(|entry| {
                format!(
                    "{}: failed to fetch origin/{}: {}",
                    entry.repo_id, entry.branch, entry.detail
                )
            })
            .collect::<Vec<_>>();
        write!(f, "{}", details.join("; "))
    }
}

impl std::error::Error for MissingRemoteBranches {}

// Confirm the exact Git diagnostic against the remote before allowing clone to recover.
fn origin_lacks_branch(repo_path: &Path, branch: &str, fetch_error: &str) -> bool {
    let wrapper = format!(
        "git fetch origin {branch} failed in {}: ",
        repo_path.display()
    );
    let Some(stderr_text) = fetch_error.strip_prefix(&wrapper) else {
        return false;
    };
    let fatal = format!("fatal: couldn't find remote ref {branch}");
    if !stderr_text.lines().any(|line| line.trim() == fatal) {
        return false;
    }
    match git_output_with_env(
        repo_path,
        [
            "ls-remote".to_string(),
            "origin".to_string(),
            format!("refs/heads/{branch}"),
        ],
        &[("LC_ALL", "C")],
    ) {
        Ok(refs) => refs.trim().is_empty(),
        Err(_) => false,
    }
}

pub(super) fn prepare_feature_branches(bundle: &ChangeGroup) -> Result<()> {
    // Fetch every repo before creating local branches: a skipped bundle stays unmaterialized.
    let mut missing = Vec::new();
    for repo in &bundle.repos {
        let Some(branch) = repo.feature_branch.as_deref() else {
            continue;
        };
        let repo_path = PathBuf::from(&repo.path);
        if git_output(&repo_path, ["remote", "get-url", "origin"]).is_err() {
            continue;
        }

        // Authentication comes from the installed Git credential helpers.
        if let Err(error) =
            git_output_with_env(&repo_path, ["fetch", "origin", branch], &[("LC_ALL", "C")])
        {
            let detail = format!("{error:#}");
            if origin_lacks_branch(&repo_path, branch, &detail) {
                missing.push(MissingRemoteBranch {
                    repo_id: repo.id.clone(),
                    branch: branch.to_string(),
                    detail,
                });
                continue;
            }
            let error = if super::clone::is_auth_shaped_failure(&detail) {
                anyhow::anyhow!("{error:#}; {}", super::credentials::NO_ACCESS_HINT)
            } else {
                error
            };
            return Err(error.context(format!("{}: failed to fetch origin/{branch}", repo.id)));
        }
        let remote_ref = format!("origin/{branch}");
        if !ref_exists(&repo_path, &remote_ref) {
            bail!("{}: fetched branch {remote_ref} was not found.", repo.id);
        }
    }
    if !missing.is_empty() {
        return Err(MissingRemoteBranches(missing).into());
    }

    for repo in &bundle.repos {
        let Some(branch) = repo.feature_branch.as_deref() else {
            continue;
        };
        let repo_path = PathBuf::from(&repo.path);
        if git_output(&repo_path, ["remote", "get-url", "origin"]).is_err() {
            continue;
        }
        let remote_ref = format!("origin/{branch}");
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

pub(super) fn request_json_with_optional_token<T: DeserializeOwned>(
    remote: &KnitRemote,
    token: Option<&str>,
    method: &str,
    path: &str,
    payload: Option<&Value>,
) -> Result<T> {
    decode_response(request_with_optional_token(
        remote, token, method, path, payload,
    )?)
}

pub(super) fn decode_response<T: DeserializeOwned>(response: HttpResponse) -> Result<T> {
    if !(200..300).contains(&response.status) {
        bail!(
            "Sync remote returned HTTP {}: {}",
            response.status,
            response.body.trim()
        );
    }
    let envelope: ApiEnvelope<T> =
        serde_json::from_str(&response.body).context("failed to parse remote response")?;
    Ok(envelope.data)
}

pub(super) fn request(
    remote: &KnitRemote,
    token: &str,
    method: &str,
    path: &str,
    payload: Option<&Value>,
) -> Result<HttpResponse> {
    request_with_optional_token(remote, Some(token), method, path, payload)
}

fn request_with_optional_token(
    remote: &KnitRemote,
    token: Option<&str>,
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
        .set("content-type", "application/json");
    let request = match token.filter(|token| !token.trim().is_empty()) {
        Some(token) => request.set("authorization", &format!("Bearer {token}")),
        None => request,
    };
    let result = match payload {
        Some(value) => {
            let body = serde_json::to_string(value).context("failed to serialize request body")?;
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
            bail!("Remote request failed for {url}: {transport}")
        }
    };
    let status = response.status();
    let body = response
        .into_string()
        .context("failed to read remote response body")?;
    Ok(HttpResponse { status, body })
}

pub(super) fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

pub(super) fn api_base_url(url: &str) -> String {
    let url = normalize_base_url(url);
    if url.ends_with("/api/v1") {
        url
    } else {
        format!("{url}/api/v1")
    }
}

/// The environment variable that overrides a named remote's stored token
/// (the name is slugified, like `token_from_env` builds it).
pub(super) fn remote_token_env_name(name: &str) -> String {
    format!(
        "KNIT_REMOTE_{}_TOKEN",
        name.chars()
            .map(|ch| if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            })
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("knit-client-test-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    fn clone_repo(source: &Path, target: &Path, bare: bool) {
        let output = Command::new("git")
            .args(["clone", "-q"])
            .args(bare.then_some("--bare"))
            .arg(source)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_source_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        run_git(path, &["init", "-q", "-b", "main"]);
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test"]);
        run_git(path, &["commit", "--allow-empty", "-q", "-m", "init"]);
    }

    fn repo_entry(id: &str, path: &Path, branch: &str) -> RepoEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "path": path,
            "baseBranch": "main",
            "featureBranch": branch,
        }))
        .unwrap()
    }

    fn bundle_with(repo: RepoEntry) -> ChangeGroup {
        let mut bundle = ChangeGroup::new(
            "feature".into(),
            "Feature".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        bundle.repos.push(repo);
        bundle
    }

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

    #[test]
    fn missing_remote_branch_is_classified_without_partial_local_branches() {
        let root = temp_dir("missing-remote");
        let seed = root.join("seed");
        init_source_repo(&seed);
        let origin = root.join("origin.git");
        clone_repo(&seed, &origin, true);
        run_git(&origin, &["branch", "knit/kept", "main"]);
        let kept = root.join("kept");
        let gone = root.join("gone");
        clone_repo(&origin, &kept, false);
        clone_repo(&origin, &gone, false);

        let mut bundle = bundle_with(repo_entry("kept-repo", &kept, "knit/kept"));
        bundle
            .repos
            .push(repo_entry("gone-repo", &gone, "knit/gone"));

        let error = prepare_feature_branches(&bundle).unwrap_err();
        let missing = error
            .downcast_ref::<MissingRemoteBranches>()
            .map(|missing| missing.0.as_slice())
            .expect("a genuinely absent branch classifies as missing");
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].repo_id, "gone-repo");
        assert_eq!(missing[0].branch, "knit/gone");
        assert_eq!(
            missing[0].summary(),
            "gone-repo: origin has no branch knit/gone"
        );
        let message = format!("{error:#}");
        assert!(
            message.contains("couldn't find remote ref knit/gone"),
            "{message}"
        );
        assert!(!branch_exists(&kept, "knit/kept"));
        assert!(!branch_exists(&gone, "knit/gone"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn missing_ref_phrase_in_a_checkout_path_never_swallows_an_auth_failure() {
        let root = temp_dir("phrase-auth");
        let seed = root.join("seed");
        init_source_repo(&seed);
        run_git(&seed, &["branch", "knit/example-feature"]);
        let origin = root.join("origin.git");
        clone_repo(&seed, &origin, true);
        let checkout = root.join("couldn't find remote ref");
        clone_repo(&origin, &checkout, false);

        let script = root.join("fake-uploadpack.sh");
        fs::write(
            &script,
            "#!/bin/sh\necho 'fatal: Authentication failed for the repository' >&2\nexit 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script, permissions).unwrap();
        }
        run_git(
            &checkout,
            &[
                "config",
                "remote.origin.uploadpack",
                script.to_str().unwrap(),
            ],
        );

        let error = prepare_feature_branches(&bundle_with(repo_entry(
            "app",
            &checkout,
            "knit/example-feature",
        )))
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<MissingRemoteBranches>()
                .map(|missing| missing.0.as_slice())
                .is_none(),
            "an authentication failure must not classify as a missing branch"
        );
        let message = format!("{error:#}");
        assert!(message.contains("Authentication failed"), "{message}");
        assert!(
            message.contains(super::super::credentials::NO_ACCESS_HINT),
            "the auth-shaped failure keeps its access hint: {message}"
        );

        fs::remove_dir_all(root).unwrap();
    }
}
