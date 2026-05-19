use crate::ids::slugify;
use crate::model::{ChangeGroup, KnitProject, KnitRemote, ProjectRepoEntry};
use crate::output as out;
use crate::store::{
    find_knit_root, load_active_bundle, load_config, project_path, read_json, save_config,
};
use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiEnvelope<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProject {
    id: String,
    slug: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteBundle {
    id: String,
    slug: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteArtifact {
    id: String,
    artifact_hash: String,
}

struct HttpResponse {
    status: u16,
    body: String,
}

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

fn workspace_config() -> Result<(std::path::PathBuf, crate::model::KnitConfig)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    Ok((root, config))
}

fn resolve_project_id(
    root: &Path,
    config: &crate::model::KnitConfig,
    name: Option<&str>,
) -> Result<String> {
    let project_id = name
        .map(slugify)
        .or_else(|| config.active_project.clone())
        .context("No project selected. Pass a project name or run `knit project init <name>`.")?;
    if !project_path(root, &project_id).exists() {
        bail!("No local Knit project named `{project_id}`.");
    }
    Ok(project_id)
}

fn resolve_remote<'a>(config: &'a crate::model::KnitConfig, name: &str) -> Result<&'a KnitRemote> {
    let remote_name = slugify(name);
    config
        .remotes
        .get(&remote_name)
        .with_context(|| format!("No KnitHub remote named `{remote_name}`. Run `knit remote add {remote_name} <url>` first."))
}

fn resolve_token(name: &str, remote: &KnitRemote) -> Result<String> {
    token_from_env(&slugify(name))
        .or_else(|| remote.token.clone())
        .context("No KnitHub token configured. Set KNITHUB_TOKEN, KNIT_REMOTE_<NAME>_TOKEN, or `knit remote token <name> <token>`.")
}

fn token_from_env(name: &str) -> Option<String> {
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

fn load_project_if_present(root: &Path, project_id: &str) -> Result<Option<KnitProject>> {
    let path = project_path(root, project_id);
    if path.exists() {
        read_json(&path).map(Some)
    } else {
        Ok(None)
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

    json!({
        "name": project_id,
        "slug": project_id,
        "visibility": "private",
        "metadata": {
            "schemaVersion": schema_version,
            "kind": kind,
            "repoCount": repo_count,
            "pushedBy": "knit"
        }
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

fn request_json<T: DeserializeOwned>(
    remote: &KnitRemote,
    token: &str,
    method: &str,
    path: &str,
    payload: Option<&Value>,
) -> Result<T> {
    decode_response(request(remote, token, method, path, payload)?)
}

fn decode_response<T: DeserializeOwned>(response: HttpResponse) -> Result<T> {
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

fn request(
    remote: &KnitRemote,
    token: &str,
    method: &str,
    path: &str,
    payload: Option<&Value>,
) -> Result<HttpResponse> {
    let url = format!("{}{}", api_base_url(&remote.url), path);
    let body = match payload {
        Some(value) => serde_json::to_string(value).context("failed to serialize request body")?,
        None => String::new(),
    };
    let mut child = Command::new("curl")
        .arg("-sS")
        .arg("-X")
        .arg(method)
        .arg("-H")
        .arg("content-type: application/json")
        .arg("-H")
        .arg(format!("authorization: Bearer {token}"))
        .arg("--data-binary")
        .arg("@-")
        .arg("--write-out")
        .arg("\nKNIT_HTTP_STATUS:%{http_code}")
        .arg(&url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(
            "failed to start curl; install curl or push from an environment that provides it",
        )?;

    child
        .stdin
        .as_mut()
        .context("failed to open curl stdin")?
        .write_all(body.as_bytes())
        .context("failed to write request body to curl")?;

    let output = child.wait_with_output().context("failed to run curl")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let (body, status) = stdout
        .rsplit_once("\nKNIT_HTTP_STATUS:")
        .context("curl did not return an HTTP status")?;
    if !output.status.success() && status.trim().is_empty() {
        bail!("curl failed: {}", stderr.trim());
    }
    let status = status
        .trim()
        .parse::<u16>()
        .context("failed to parse HTTP status from curl")?;
    if status == 000 {
        bail!("curl failed: {}", stderr.trim());
    }
    Ok(HttpResponse {
        status,
        body: body.to_string(),
    })
}

fn normalize_base_url(url: &str) -> String {
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
