use crate::checkout::is_in_place;
use crate::commands::agents::{print_worktree_agents_summary, write_worktree_agents_md};
use crate::commands::worktree::materialize_repos;
use crate::git::{
    branch_exists, current_branch, git_output, is_ancestor, is_git_worktree, ref_exists, rev_parse,
};
use crate::ids::slugify;
use crate::model::{
    ChangeGroup, KnitConfig, KnitProject, KnitRemote, ProjectRepoEntry, CHECKOUT_MODE_WORKTREE,
    SCHEMA_VERSION,
};
use crate::output as out;
use crate::store::{
    bundle_path, find_knit_root, load_active_bundle, load_active_bundle_for_update, load_config,
    project_path, read_json, save_active_bundle, save_config, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProjectExport {
    project: RemoteExportProject,
    knit_project: Option<KnitProject>,
    repositories: Vec<RemoteExportRepository>,
    bundles: Vec<RemoteExportBundle>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportProject {
    slug: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportRepository {
    local_id: Option<String>,
    name: String,
    default_branch: Option<String>,
    remote_url: Option<String>,
    metadata: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportBundle {
    slug: String,
    lifecycle_state: String,
    current_artifact: Option<RemoteExportArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteExportArtifact {
    artifact_hash: String,
    payload: Value,
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
        remotes,
    };
    save_config(&target_root, &config)?;

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

pub fn pull_remote_state(remote_name: Option<&str>, skip_remote: bool) -> Result<()> {
    if skip_remote {
        return Ok(());
    }

    let mut active = load_active_bundle_for_update()?;
    let config = load_config(&active.root)?;
    let Some(remote_name) = remote_name.map(slugify).or(config.sync_remote.clone()) else {
        return Ok(());
    };
    let remote = resolve_remote(&config, &remote_name)?;
    let token = resolve_token(&remote_name, remote)?;
    let project_id = active
        .bundle
        .project_id
        .clone()
        .or(config.active_project.clone())
        .context("No project selected for remote pull. Set activeProject or pass a bundle with projectId.")?;
    let project = load_project_if_present(&active.root, &project_id)?
        .with_context(|| format!("No local Knit project named `{project_id}`."))?;
    let export = fetch_project_export(remote, &token, &project_id)?;
    let remote_bundle = export
        .bundles
        .iter()
        .find(|bundle| bundle.slug == active.bundle.id)
        .with_context(|| {
            format!(
                "Remote project `{project_id}` has no bundle `{}`.",
                active.bundle.id
            )
        })?;
    let artifact = remote_bundle.current_artifact.as_ref().with_context(|| {
        format!(
            "Remote bundle `{}` has no current artifact.",
            active.bundle.id
        )
    })?;
    let remote_payload = decode_bundle_payload(&artifact.payload, &remote_bundle.slug)?;
    let localized = localize_bundle(remote_payload, &project)?;

    prepare_feature_branches(&localized)?;
    ensure_remote_bundle_fast_forward(&active.bundle, &localized)?;
    active.bundle = localized;
    materialize_repos(&mut active, None)?;
    fast_forward_feature_checkouts(&mut active)?;
    save_active_bundle(&active)?;

    println!(
        "{} {} {}",
        out::movement("pulled"),
        out::repo(&remote_bundle.slug),
        out::muted(&artifact.artifact_hash)
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

fn fetch_project_export(
    remote: &KnitRemote,
    token: &str,
    project_identifier: &str,
) -> Result<RemoteProjectExport> {
    request_json(
        remote,
        token,
        "GET",
        &format!("/projects/{}/export", slugify(project_identifier)),
        None,
    )
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

fn clone_export_repositories(
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
        let repo_path = repo_paths
            .get(&local_id)
            .with_context(|| format!("{local_id}: repository was not cloned"))?;
        project.repos.push(ProjectRepoEntry {
            id: local_id,
            path: repo_path.to_string_lossy().to_string(),
            remote: repository.remote_url.clone(),
            base_branch: repository
                .default_branch
                .clone()
                .filter(|branch| !branch.trim().is_empty())
                .unwrap_or_else(|| "main".to_string()),
            checkout_mode: metadata_string(&repository.metadata, "checkoutMode")
                .unwrap_or_else(|| CHECKOUT_MODE_WORKTREE.to_string()),
            include_by_default: metadata_bool(&repository.metadata, "includeByDefault")
                .unwrap_or(true),
        });
    }

    project.updated_at = now_iso();
    Ok(project)
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

fn decode_bundle_payload(payload: &Value, bundle_slug: &str) -> Result<ChangeGroup> {
    serde_json::from_value(payload.clone()).with_context(|| {
        format!("Remote bundle `{bundle_slug}` does not contain a supported Knit bundle payload.")
    })
}

fn localize_bundle(mut bundle: ChangeGroup, project: &KnitProject) -> Result<ChangeGroup> {
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
        repo.checkout_mode = local.checkout_mode.clone();
        repo.worktree_path = None;
    }
    Ok(bundle)
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
    save_active_bundle(&active)
}

fn prepare_feature_branches(bundle: &ChangeGroup) -> Result<()> {
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

fn ensure_remote_bundle_fast_forward(local: &ChangeGroup, remote: &ChangeGroup) -> Result<()> {
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

fn fast_forward_feature_checkouts(active: &mut ActiveBundle) -> Result<()> {
    let root = active.root.clone();
    for repo in &mut active.bundle.repos {
        let Some(branch) = repo.feature_branch.as_deref() else {
            continue;
        };
        let Some(checkout) = remote_checkout_dir(&root, repo) else {
            continue;
        };
        let actual = current_branch(&checkout)?.unwrap_or_else(|| "(detached HEAD)".to_string());
        if actual != branch {
            bail!(
                "{}: expected feature checkout branch `{branch}`, found `{actual}` in {}.",
                repo.id,
                checkout.display()
            );
        }
        let remote_ref = format!("origin/{branch}");
        if ref_exists(&checkout, &remote_ref) {
            git_output(&checkout, ["merge", "--ff-only", &remote_ref])
                .with_context(|| format!("{}: failed to fast-forward {branch}", repo.id))?;
        }
        repo.head_sha = Some(
            rev_parse(&checkout, "HEAD")
                .with_context(|| format!("{}: failed to read feature checkout HEAD", repo.id))?,
        );
    }
    active.bundle.updated_at = now_iso();
    Ok(())
}

fn remote_checkout_dir(root: &Path, repo: &crate::model::RepoEntry) -> Option<PathBuf> {
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

fn export_repo_local_id(repository: &RemoteExportRepository) -> String {
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
    let mut command = Command::new("curl");
    command
        .arg("-sS")
        .arg("-X")
        .arg(method)
        .arg("-H")
        .arg("content-type: application/json")
        .arg("-H")
        .arg(format!("authorization: Bearer {token}"));
    if payload.is_some() {
        command.arg("--data-binary").arg("@-").stdin(Stdio::piped());
    }
    let mut child = command
        .arg("--write-out")
        .arg("\nKNIT_HTTP_STATUS:%{http_code}")
        .arg(&url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start curl; install curl or use an environment that provides it")?;

    if payload.is_some() {
        child
            .stdin
            .as_mut()
            .context("failed to open curl stdin")?
            .write_all(body.as_bytes())
            .context("failed to write request body to curl")?;
    }

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
