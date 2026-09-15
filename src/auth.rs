//! Personal forge credentials and explicit project/repository assignments.
//!
//! These files are deliberately separate from portable project and bundle artifacts.
use crate::model::{ChangeGroup, KnitProject};
use crate::store;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

static PROJECT_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

pub fn set_project_override(project: Option<String>) {
    *PROJECT_OVERRIDE
        .lock()
        .expect("project override lock poisoned") = project;
}

/// The currently selected project override, for callers that temporarily
/// change it and must restore the previous value afterwards.
pub fn current_project_override() -> Option<String> {
    PROJECT_OVERRIDE
        .lock()
        .expect("project override lock poisoned")
        .clone()
}

/// Repo id → remote URL for repositories the project's full membership knows
/// but this workspace has not materialized: out of a clone scope, or a pull
/// add that failed before the checkout existed. Local-only bookkeeping that
/// keeps those references discoverable — guided setup validates against
/// them and the credential resolver can serve them once a binding exists —
/// without ever creating an assignment silently. Never exported, and never
/// a place for secrets.
pub fn known_pending_repos_path(root: &Path, project: &str) -> PathBuf {
    store::project_path(root, project).with_file_name(format!("{project}.known-repos.json"))
}

pub fn load_known_pending_repos(root: &Path, project: &str) -> BTreeMap<String, String> {
    #[derive(serde::Deserialize, Default)]
    struct Known {
        #[serde(default)]
        repos: BTreeMap<String, String>,
    }
    fs::read(known_pending_repos_path(root, project))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Known>(&bytes).ok())
        .map(|known| known.repos)
        .unwrap_or_default()
}

pub fn save_known_pending_repos(
    root: &Path,
    project: &str,
    repos: &BTreeMap<String, String>,
) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Known<'a> {
        repos: &'a BTreeMap<String, String>,
    }
    let path = known_pending_repos_path(root, project);
    if repos.is_empty() {
        // Absent file and empty file mean the same thing; keep the
        // workspace free of bookkeeping nobody needs.
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    store::write_json(&path, &Known { repos })
}

/// Serialize read-modify-write operations across Knit processes.
pub fn lock() -> Result<store::KnitLock> {
    let path = personal_path("forge-auth.json")?;
    let parent = path.parent().context("private config has no parent")?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    store::acquire_named_lock(parent, "forge-auth")
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialSpec {
    pub provider: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// Which of the provider's token kinds this credential is, when known.
    /// Never inferred from the opaque token value: an unknown stays `None`
    /// until the person who owns the credential classifies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStore {
    #[serde(default)]
    pub credentials: BTreeMap<String, CredentialSpec>,
    /// Canonical project artifact path -> repository id -> credential name.
    #[serde(default)]
    pub projects: BTreeMap<String, BTreeMap<String, String>>,
}

/// Intentionally does not implement Debug or Serialize: it contains a secret.
pub struct ResolvedCredential {
    pub name: String,
    pub provider: String,
    pub host: String,
    pub username: String,
    pub token: String,
}

impl ResolvedCredential {
    pub fn redact(&self, text: &str) -> String {
        if self.token.is_empty() {
            text.to_owned()
        } else {
            text.replace(&self.token, "[REDACTED]")
        }
    }

    pub fn git_username(&self) -> String {
        if self.provider == "github" {
            "x-access-token".into()
        } else if self.provider == "bitbucket" {
            if self.username.is_empty() {
                "x-token-auth".into()
            } else {
                "x-bitbucket-api-token-auth".into()
            }
        } else if !self.username.is_empty() {
            self.username.clone()
        } else {
            "oauth2".into()
        }
    }
}

fn personal_path(name: &str) -> Result<PathBuf> {
    Ok(store::global_config_path()?
        .parent()
        .context("global config has no parent directory")?
        .join(name))
}

pub fn load() -> Result<AuthStore> {
    load_file(&personal_path("forge-auth.json")?)
}

fn load_file<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid personal forge credential configuration")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).context("Cannot read personal forge credential configuration"),
    }
}

/// Hosts are case-insensitive domain names; store the canonical lowercase
/// form so exact comparisons elsewhere stay sound, whatever case a group,
/// CLI flag, or older store entry carried.
fn normalize_store(value: &mut AuthStore) {
    for spec in value.credentials.values_mut() {
        spec.host = spec.host.to_ascii_lowercase();
    }
}

fn validate_store(value: &mut AuthStore) -> Result<()> {
    normalize_store(value);
    for (name, spec) in &value.credentials {
        validate_name(name)?;
        validate_spec(spec)?;
    }
    Ok(())
}

pub fn save(value: &AuthStore) -> Result<()> {
    let mut normalized = value.clone();
    validate_store(&mut normalized)?;
    write_private(&personal_path("forge-auth.json")?, &normalized)
}

/// Save one credential and its token-source transition while the caller holds
/// the auth lock. Validate and stage everything before changing either file.
/// The secret is replaced last: a failed save leaves the old token untouched.
pub fn save_credential(value: &AuthStore, name: &str, token: Option<&str>) -> Result<()> {
    let mut normalized = value.clone();
    validate_store(&mut normalized)?;
    validate_name(name)?;
    let spec = normalized
        .credentials
        .get(name)
        .context("Credential is not configured")?;
    if spec.token_env.is_some() == token.is_some() {
        bail!("Credential must use either an environment reference or a saved token");
    }
    if let Some(token) = token {
        validate_token(token)?;
    }
    let registry_path = personal_path("forge-auth.json")?;
    let secrets_path = personal_path("forge-secrets.json")?;
    let mut secrets: BTreeMap<String, String> = load_file(&secrets_path)?;
    match token {
        Some(token) => {
            secrets.insert(name.to_owned(), token.to_owned());
        }
        None => {
            secrets.remove(name);
        }
    }
    let rollback = match fs::read(&registry_path) {
        Ok(bytes) => Some(prepare_private_bytes(&registry_path, &bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).context("Cannot read previous forge credential configuration")
        }
    };
    let registry = prepare_private(&registry_path, &normalized)?;
    let secrets = prepare_private(&secrets_path, &secrets)?;
    commit_registry_and_secret(registry, secrets, rollback)
}

fn commit_registry_and_secret(
    registry: PreparedPrivate,
    secrets: PreparedPrivate,
    rollback: Option<PreparedPrivate>,
) -> Result<()> {
    registry.commit()?;
    if let Err(error) = secrets.commit() {
        let restored = match rollback {
            Some(previous) => previous.commit(),
            None => fs::remove_file(&registry.path)
                .context("Cannot remove new forge credential configuration"),
        };
        if restored.is_err() {
            bail!("Could not save forge credentials. The previous token is unchanged, but restoring credential metadata failed; check personal forge configuration before retrying");
        }
        return Err(error)
            .context("Could not save forge credentials; previous credentials were restored");
    }
    Ok(())
}

pub fn save_token(name: &str, token: &str) -> Result<()> {
    validate_name(name)?;
    validate_token(token)?;
    let path = personal_path("forge-secrets.json")?;
    let mut secrets: BTreeMap<String, String> = load_file(&path)?;
    secrets.insert(name.to_owned(), token.to_owned());
    write_private(&path, &secrets)
}

pub fn remove_token(name: &str) -> Result<()> {
    let path = personal_path("forge-secrets.json")?;
    let mut secrets: BTreeMap<String, String> = load_file(&path)?;
    secrets.remove(name);
    write_private(&path, &secrets)
}

struct PreparedPrivate {
    path: PathBuf,
    temporary: PathBuf,
}

impl PreparedPrivate {
    fn commit(&self) -> Result<()> {
        // rename replaces files atomically, including on Windows. Never remove
        // the destination first: a failed rename must retain the old token.
        fs::rename(&self.temporary, &self.path).context("Cannot save private forge config")
    }
}

impl Drop for PreparedPrivate {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

fn write_private<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    prepare_private(path, value)?.commit()
}

fn prepare_private<T: Serialize>(path: &Path, value: &T) -> Result<PreparedPrivate> {
    let mut bytes = serde_json::to_vec_pretty(value).context("Cannot serialize forge config")?;
    bytes.push(b'\n');
    prepare_private_bytes(path, &bytes)
}

fn prepare_private_bytes(path: &Path, bytes: &[u8]) -> Result<PreparedPrivate> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let parent = path.parent().context("private config has no parent")?;
    fs::create_dir_all(parent).context("Cannot create personal forge credential directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let temporary = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .context("Cannot create private forge config")?;
    let prepared = PreparedPrivate {
        path: path.to_owned(),
        temporary,
    };
    let result = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    result?;
    Ok(prepared)
}

#[cfg(test)]
mod storage_transaction_tests {
    use super::*;

    #[test]
    fn failed_staged_commit_restores_registry_and_preserves_secret() {
        // Exercise real rename failures after staging, without environment
        // overrides or a production failure-injection hook.
        let root =
            std::env::temp_dir().join(format!("knit-auth-transaction-{}", std::process::id()));
        for existed in [false, true] {
            for fail_registry in [false, true] {
                let directory = root.join(format!("{existed}-{fail_registry}"));
                fs::create_dir_all(&directory).unwrap();
                let registry_path = directory.join("forge-auth.json");
                let secrets_path = directory.join("forge-secrets.json");
                let old_registry = b"{\"credentials\": {}}\n";
                let old_secret = b"{\"selected\":\"old-synthetic-secret\"}\n";
                if existed {
                    fs::write(&registry_path, old_registry).unwrap();
                }
                fs::write(&secrets_path, old_secret).unwrap();
                let rollback =
                    existed.then(|| prepare_private_bytes(&registry_path, old_registry).unwrap());
                let registry = prepare_private_bytes(&registry_path, b"new-registry").unwrap();
                let secrets =
                    prepare_private_bytes(&secrets_path, b"new-synthetic-secret").unwrap();
                fs::remove_file(if fail_registry {
                    &registry.temporary
                } else {
                    &secrets.temporary
                })
                .unwrap();
                let error = commit_registry_and_secret(registry, secrets, rollback).unwrap_err();
                assert!(!format!("{error:#}").contains("synthetic-secret"));
                assert_eq!(fs::read(&secrets_path).unwrap(), old_secret);
                if existed {
                    assert_eq!(fs::read(&registry_path).unwrap(), old_registry);
                } else {
                    assert!(!registry_path.exists());
                }
                assert!(fs::read_dir(&directory).unwrap().all(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .unwrap()
                    == "json"));
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("Credential names must contain only letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

pub fn validate_spec(spec: &CredentialSpec) -> Result<()> {
    if !matches!(
        spec.provider.as_str(),
        "github" | "gitlab" | "bitbucket" | "forgejo" | "codeberg"
    ) {
        bail!("Unsupported forge credential provider");
    }
    validate_host(&spec.host)?;
    if spec
        .username
        .as_deref()
        .is_some_and(|v| v.contains(':') || v.chars().any(char::is_control))
    {
        bail!("Credential username contains invalid characters");
    }
    if let Some(env) = &spec.token_env {
        if env.is_empty()
            || env.starts_with(|c: char| c.is_ascii_digit())
            || !env.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            bail!("Token environment variable must be a valid environment variable name");
        }
    }
    if let Some(token_type) = &spec.token_type {
        let supported = crate::model::token_types_for_provider(&spec.provider);
        if !supported.contains(&token_type.as_str()) {
            bail!(
                "Token type `{token_type}` is not one of the {} token kinds this provider offers",
                spec.provider
            );
        }
    }
    Ok(())
}

/// Full semantic validation of a project's auth requirements: structure, plus
/// resolution of every referenced repository against the project's repos.
/// Repository references must exist and sit on the group's forge (matching
/// host, and matching provider whenever the remote identifies one — the URL
/// is authoritative for known forges, a declared provider covers custom
/// hosts). Callers that import a deliberately partial project — a scoped
/// clone, or a pull whose new repo failed to clone — validate against the
/// full membership instead, via [`validate_project_auth_with_pending`].
pub fn validate_project_auth(project: &KnitProject) -> Result<()> {
    validate_project_auth_with_pending(project, &BTreeMap::new())
}

/// Like [`validate_project_auth`], but repository references missing from
/// the local project may resolve against `pending`: repo id → remote URL for
/// repositories known to the project's full membership but not present in
/// this workspace yet (out of clone scope, or not yet cloned). A reference
/// that is neither local nor pending is a typo and fails loudly.
pub fn validate_project_auth_with_pending(
    project: &KnitProject,
    pending: &BTreeMap<String, String>,
) -> Result<()> {
    let Some(auth) = &project.auth else {
        return Ok(());
    };
    auth.validate_structure()
        .map_err(|error| anyhow::anyhow!("Invalid project auth requirements: {error}"))?;
    // A project with ambiguous local ids can never match a group reliably.
    let mut local_ids = std::collections::BTreeSet::new();
    for repo in &project.repos {
        if !local_ids.insert(repo.id.clone()) {
            bail!(
                "Invalid project auth requirements: project lists repository `{}` more than once",
                repo.id
            );
        }
    }
    for group in &auth.groups {
        for repo_id in &group.repos {
            let remote = match project.repos.iter().find(|repo| &repo.id == repo_id) {
                Some(repo) => match &repo.remote {
                    Some(remote) => Some(remote.clone()),
                    None => bail!(
                        "Invalid project auth requirements: group `{}` references repository `{repo_id}`, which has no forge remote",
                        group.id
                    ),
                },
                None => pending.get(repo_id).cloned(),
            };
            let Some(remote) = remote else {
                bail!(
                    "Invalid project auth requirements: group `{}` references unknown repository `{repo_id}`; this workspace lists {}",
                    group.id,
                    if project.repos.is_empty() {
                        "no repositories".to_string()
                    } else {
                        project.repos.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(", ")
                    }
                );
            };
            validate_auth_group_remote(group, repo_id, &remote)?;
        }
    }
    Ok(())
}

fn validate_auth_group_remote(
    group: &crate::model::ProjectAuthGroup,
    repo_id: &str,
    remote: &str,
) -> Result<()> {
    let target = remote_target(remote).map_err(|_| {
        anyhow::anyhow!(
            "Invalid project auth requirements: group `{}` references repository `{repo_id}`, whose remote is not a supported forge remote",
            group.id
        )
    })?;
    if !target.0.eq_ignore_ascii_case(&group.host) {
        bail!(
            "Invalid project auth requirements: group `{}` is for host `{}` but repository `{repo_id}` is on `{}`",
            group.id,
            group.host,
            target.0
        );
    }
    let declared = crate::providers::for_remote(remote)
        .map(|forge| forge.id().to_owned())
        .unwrap_or_default();
    let declared = match declared.as_str() {
        "codeberg" | "gitea" => "forgejo".to_owned(),
        value => value.to_owned(),
    };
    // Known forges identify themselves by host; a custom host with no
    // recognizable name trusts the group's declared provider.
    if !declared.is_empty() && declared != group.provider {
        bail!(
            "Invalid project auth requirements: group `{}` declares provider `{}` but repository `{repo_id}` is a `{declared}` remote",
            group.id,
            group.provider
        );
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<()> {
    if token.trim().is_empty() || token.chars().any(char::is_whitespace) || token.contains('\0') {
        bail!("Forge token must be nonempty and contain no whitespace or NUL characters");
    }
    Ok(())
}

pub fn credential(name: &str) -> Result<ResolvedCredential> {
    validate_name(name)?;
    let registry = load()?;
    let spec = registry
        .credentials
        .get(name)
        .with_context(|| format!("Credential `{name}` is not configured; run `knit auth add`"))?;
    validate_spec(spec)?;
    let token = if let Some(variable) = &spec.token_env {
        std::env::var(variable).with_context(|| {
            format!("Credential `{name}` requires environment variable `{variable}`")
        })?
    } else {
        let secrets: BTreeMap<String, String> = load_file(&personal_path("forge-secrets.json")?)?;
        secrets.get(name).cloned().with_context(|| {
            format!("Credential `{name}` has no saved token; run `knit auth add`")
        })?
    };
    validate_token(&token)?;
    Ok(ResolvedCredential {
        name: name.to_owned(),
        provider: spec.provider.clone(),
        host: spec.host.to_ascii_lowercase(),
        username: spec.username.clone().unwrap_or_default(),
        token,
    })
}

pub fn project_key(root: &Path, project: &str) -> Result<String> {
    validate_name(project).context("Invalid project id")?;
    Ok(store::project_path(root, project)
        .canonicalize()
        .context("Cannot locate Knit project artifact")?
        .to_string_lossy()
        .into_owned())
}

pub fn project_context(cwd: &Path, explicit: Option<&str>) -> Result<(PathBuf, KnitProject)> {
    optional_project_context(cwd, explicit)?.context("No active Knit project; pass --project <id>")
}

fn optional_project_context(
    cwd: &Path,
    explicit: Option<&str>,
) -> Result<Option<(PathBuf, KnitProject)>> {
    let root = context_root(cwd).context("No Knit workspace found for forge credential setup")?;
    let config = store::load_config(&root)?;
    let selected = explicit.map(str::to_owned).or_else(|| {
        PROJECT_OVERRIDE
            .lock()
            .expect("project override lock poisoned")
            .clone()
    });
    let project = if let Some(project) = selected {
        project
    } else {
        let process_cwd = std::env::current_dir()?;
        let process_root = store::find_knit_root(&process_cwd);
        let context_cwd = if store::infer_worktree_bundle(&root, cwd).is_some() {
            cwd.to_path_buf()
        } else if process_root.as_ref() == Some(&root) {
            // Git may operate on a source checkout while Knit was invoked from
            // a bundle worktree. Keep that invocation's project selection.
            process_cwd
        } else {
            cwd.to_path_buf()
        };
        let bundle_project = match store::resolve_bundle_id(&root, &context_cwd, &config) {
            Ok((id, source)) if source != store::BundleResolutionSource::Config => {
                let bundle: ChangeGroup = store::read_json(&store::bundle_path(&root, &id))?;
                Some(bundle.project_id)
            }
            Ok(_) => None,
            Err(error) if error.to_string().starts_with("No active Knit bundle") => None,
            Err(error) => return Err(error),
        };
        let Some(project) = project_from_bundle(bundle_project, config.active_project) else {
            return Ok(None);
        };
        project
    };
    validate_name(&project).context("Invalid project id")?;
    let value = store::read_json(&store::project_path(&root, &project))?;
    Ok(Some((root, value)))
}

fn project_from_bundle(
    bundle_project: Option<Option<String>>,
    active_project: Option<String>,
) -> Option<String> {
    // An explicitly selected ad-hoc bundle has no project, even if the
    // workspace also happens to have an active reusable project.
    bundle_project.unwrap_or(active_project)
}

fn context_root(cwd: &Path) -> Option<PathBuf> {
    store::find_knit_root(cwd).or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| store::find_knit_root(&p))
    })
}

/// Normalize a remote to (host, repository path), rejecting embedded secrets and
/// ambiguous paths. Invalid inputs are never included in error messages.
pub fn remote_target(remote: &str) -> Result<(String, String)> {
    if remote.chars().any(char::is_whitespace) || remote.contains(['\\', '%', '?', '#', '\0']) {
        bail!("Invalid forge remote URL");
    }
    let (host, path) = if remote.contains("://") {
        let authority = remote
            .split_once("://")
            .unwrap()
            .1
            .split('/')
            .next()
            .unwrap_or("");
        if authority.rsplit('@').next().unwrap_or("").contains(':') {
            bail!("Custom ports are not supported for forge credentials");
        }
        // Reject traversal before URL parsing can normalize it away.
        if remote.split('/').any(|s| s == "." || s == "..") {
            bail!("Invalid forge remote path");
        }
        let url =
            url::Url::parse(remote).map_err(|_| anyhow::anyhow!("Invalid forge remote URL"))?;
        if !matches!(url.scheme(), "https" | "ssh")
            || url.port().is_some()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.username().is_empty() || url.scheme() == "ssh" && url.username() == "git")
        {
            bail!(
                "Forge remotes must use HTTPS or SSH without embedded credentials or custom ports"
            );
        }
        (
            url.host_str()
                .context("Forge remote has no host")?
                .to_owned(),
            url.path().to_owned(),
        )
    } else {
        let (authority, path) = remote
            .split_once(':')
            .context("Expected an HTTPS or SSH forge remote")?;
        let host = authority.strip_prefix("git@").unwrap_or(authority);
        if host.contains('@') {
            bail!("SSH forge remotes may use only the git username");
        }
        (host.to_owned(), path.to_owned())
    };
    validate_host(&host)?;
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.split('/').count() < 2
        || path.split('/').any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || !segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
    {
        bail!("Invalid forge repository path");
    }
    Ok((host.to_ascii_lowercase(), path.to_owned()))
}

fn validate_host(host: &str) -> Result<()> {
    if host.is_empty()
        || host.starts_with('.')
        || host.ends_with('.')
        || host
            .split('.')
            .any(|s| s.is_empty() || s.starts_with('-') || s.ends_with('-'))
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
    {
        bail!("Invalid forge host; provide a hostname without scheme, port, or path");
    }
    Ok(())
}

/// Resolve a host-less explicit forge target against the selected project's
/// recorded remotes. Repository paths are not globally unique: matching paths
/// on two hosts require a full remote, never a guess from the checkout origin.
pub fn resolve_repository(
    cwd: &Path,
    provider: &str,
    repository: &str,
) -> Result<Option<ResolvedCredential>> {
    let registry = load()?;
    if registry.projects.values().all(BTreeMap::is_empty) || context_root(cwd).is_none() {
        return Ok(None);
    }
    let Some((root, project)) = optional_project_context(cwd, None)? else {
        return Ok(None);
    };
    let key = project_key(&root, &project.id)?;
    let Some(bindings) = registry.projects.get(&key).filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    let canonical_provider = |value: &str| match value {
        "codeberg" | "gitea" => "forgejo".to_owned(),
        value => value.to_owned(),
    };
    let mut matches = Vec::new();
    for repo in &project.repos {
        let Some(remote) = repo.remote.as_deref() else {
            continue;
        };
        let Ok((_, path)) = remote_target(remote) else {
            continue;
        };
        if path != repository {
            continue;
        }
        // Known hosts identify their provider directly. An explicitly assigned
        // provider identifies custom hosts; an unassigned unknown host remains
        // a candidate so a partial assignment cannot hide an ambiguity.
        let repo_provider = crate::providers::for_remote(remote)
            .map(|forge| forge.id().to_owned())
            .or_else(|| {
                bindings
                    .get(&repo.id)
                    .and_then(|name| registry.credentials.get(name))
                    .map(|spec| canonical_provider(&spec.provider))
            });
        if repo_provider.is_some_and(|value| value != canonical_provider(provider)) {
            continue;
        }
        matches.push(remote);
    }
    let remote = match matches.as_slice() {
        [remote] => *remote,
        [] => bail!("Explicit repository is not configured for this forge in the selected project; refusing to select a credential"),
        _ => bail!("Several project remotes match this explicit repository; credential assignment is ambiguous"),
    };
    resolve(cwd, Some(remote))
}

/// Filesystem remotes never consume forge credentials, including Windows drive paths.
pub fn is_local_remote(remote: &str) -> bool {
    !remote.contains(':')
        || remote.starts_with("file://")
        || Path::new(remote).is_absolute()
        || (remote.as_bytes().get(1) == Some(&b':')
            && remote
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
            && remote
                .as_bytes()
                .get(2)
                .is_some_and(|b| *b == b'/' || *b == b'\\'))
}

/// Discover the assigned adapter without reading a token or secret store.
/// Use the explicit operation remote, never the current checkout's origin.
pub(crate) fn provider_for_remote(cwd: &Path, remote: &str) -> Result<Option<String>> {
    let registry = load()?;
    if registry.projects.values().all(BTreeMap::is_empty) || context_root(cwd).is_none() {
        return Ok(None);
    }
    let Some((root, project)) = optional_project_context(cwd, None)? else {
        return Ok(None);
    };
    let key = project_key(&root, &project.id)?;
    let Some(bindings) = registry.projects.get(&key).filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    if is_local_remote(remote) {
        return Ok(None);
    }
    let target = remote_target(remote)?;
    let pending = load_known_pending_repos(&root, &project.id);
    let Some(name) = binding_for_target(&registry, bindings, &project, &target, &pending)? else {
        return Ok(None);
    };
    // binding_for_target validates the credential metadata and exact host.
    Ok(Some(registry.credentials[name].provider.clone()))
}

pub fn resolve(cwd: &Path, remote: Option<&str>) -> Result<Option<ResolvedCredential>> {
    let registry = load()?;
    if registry.projects.values().all(BTreeMap::is_empty) || context_root(cwd).is_none() {
        return Ok(None);
    }
    let Some((root, project)) = optional_project_context(cwd, None)? else {
        return Ok(None);
    };
    let key = project_key(&root, &project.id)?;
    let Some(bindings) = registry.projects.get(&key).filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    let origin;
    let remote = match remote {
        Some(remote) => remote,
        None => {
            let output = Command::new("git")
                .args(["remote", "get-url", "origin"])
                .current_dir(cwd)
                .output()
                .context("Cannot inspect repository origin for forge credentials")?;
            if !output.status.success() {
                return Ok(None);
            }
            origin = String::from_utf8(output.stdout).context("Repository origin is not UTF-8")?;
            origin.trim()
        }
    };
    // A filesystem remote cannot receive a forge secret, and remains usable in
    // projects that also contain authenticated forge repositories.
    if is_local_remote(remote) {
        return Ok(None);
    }
    let target = remote_target(remote)?;
    let pending = load_known_pending_repos(&root, &project.id);
    let Some(name) = binding_for_target(&registry, bindings, &project, &target, &pending)? else {
        return Ok(None);
    };
    let resolved = credential(name)?;
    if resolved.host != target.0 {
        bail!("Assigned credential host does not match the repository host");
    }
    Ok(Some(resolved))
}

fn binding_for_target<'a>(
    registry: &'a AuthStore,
    bindings: &'a BTreeMap<String, String>,
    project: &KnitProject,
    target: &(String, String),
    pending: &BTreeMap<String, String>,
) -> Result<Option<&'a str>> {
    let matches: Vec<_> = project
        .repos
        .iter()
        .filter(|repo| {
            repo.remote
                .as_deref()
                .and_then(|remote| remote_target(remote).ok())
                .as_ref()
                == Some(target)
        })
        .collect();
    if matches.len() > 1 {
        bail!("Several project repositories match this forge remote; credential assignment is ambiguous");
    }
    if let Some(repo) = matches.first() {
        let name = bindings.get(&repo.id).with_context(|| {
            format!(
                "Project `{}` repository `{}` has no assigned credential; run `knit auth setup`",
                project.id, repo.id
            )
        })?;
        return validated_binding(registry, name, &target.0);
    }
    // No tracked repository matches. A repository the full membership knows
    // but this workspace has not materialized yet (out of clone scope, or a
    // pull add that failed) can still be served by its explicit binding —
    // the pull retry after `knit auth setup` depends on it. Ambiguous
    // pending candidates fail instead of matching any one of them.
    let pending_matches: Vec<_> = pending
        .iter()
        .filter(|(_, remote)| remote_target(remote).ok().as_ref() == Some(target))
        .collect();
    if pending_matches.len() > 1 {
        bail!("Several pending repositories match this forge remote; credential assignment is ambiguous");
    }
    if let Some((repo_id, _)) = pending_matches.first() {
        let name = bindings.get(repo_id.as_str()).with_context(|| {
            format!(
                "Project `{}` repository `{repo_id}` has no assigned credential yet; run `knit auth setup`, then `knit pull` to clone it",
                project.id
            )
        })?;
        return validated_binding(registry, name, &target.0);
    }
    bail!("Remote is not a configured repository in this project; refusing to select a forge credential")
}

fn validated_binding<'a>(
    registry: &'a AuthStore,
    name: &'a str,
    host: &str,
) -> Result<Option<&'a str>> {
    validate_name(name)?;
    let spec = registry
        .credentials
        .get(name)
        .with_context(|| format!("Assigned credential `{name}` is not configured"))?;
    validate_spec(spec)?;
    if !spec.host.eq_ignore_ascii_case(host) {
        bail!("Assigned credential host does not match the repository host");
    }
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> KnitProject {
        serde_json::from_value(serde_json::json!({
            "schemaVersion": "1", "kind": "KnitProject", "id": "one", "createdAt": "", "updatedAt": "",
            "repos": [{"id":"app", "path":"app", "remote":"git@github.com:org/app.git", "baseBranch":"main"},
                      {"id":"lib", "path":"lib", "remote":"https://github.com/org/lib", "baseBranch":"main"}]
        })).unwrap()
    }

    #[test]
    fn equivalent_https_and_ssh_remotes() {
        let expected = ("github.com".into(), "org/app".into());
        for remote in [
            "https://GitHub.com/org/app.git",
            "git@github.com:org/app.git",
            "ssh://git@github.com/org/app/",
        ] {
            assert_eq!(remote_target(remote).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_secret_bearing_and_ambiguous_remotes_without_echo() {
        for remote in [
            "https://SECRET@github.com/org/app",
            "https://u:SECRET@github.com/org/app",
            "https://github.com/org/../app",
            "https://github.com/org/%2e%2e/app",
            "https://github.com:444/org/app",
            "https://github.com:443/org/app",
            "https://github.com/org/app?SECRET",
            "user@github.com:org/app",
            "https://github.com/org//app",
        ] {
            let error = remote_target(remote).unwrap_err().to_string();
            assert!(!error.contains("SECRET"));
        }
    }

    #[test]
    fn partial_project_assignments_never_fall_back() {
        let mut registry = AuthStore::default();
        registry.credentials.insert(
            "work".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "github.com".into(),
                username: None,
                token_env: None,
                token_type: None,
            },
        );
        let bindings = BTreeMap::from([("app".into(), "work".into())]);
        assert_eq!(
            binding_for_target(
                &registry,
                &bindings,
                &project(),
                &remote_target("https://github.com/org/app").unwrap(),
                &BTreeMap::new()
            )
            .unwrap(),
            Some("work")
        );
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &remote_target("https://github.com/org/lib").unwrap(),
            &BTreeMap::new()
        )
        .is_err());
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &remote_target("https://gitlab.com/other/app").unwrap(),
            &BTreeMap::new()
        )
        .is_err());
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &remote_target("https://github.com/other/app").unwrap(),
            &BTreeMap::new()
        )
        .is_err());
    }

    #[test]
    fn same_remote_uses_only_its_projects_assignment() {
        let spec = CredentialSpec {
            provider: "github".into(),
            host: "github.com".into(),
            username: None,
            token_env: None,
            token_type: None,
        };
        let mut registry = AuthStore::default();
        registry.credentials.insert("narrow".into(), spec.clone());
        registry.credentials.insert("classic".into(), spec);
        let target = remote_target("https://github.com/org/app").unwrap();
        for name in ["narrow", "classic"] {
            let bindings = BTreeMap::from([("app".into(), name.into())]);
            assert_eq!(
                binding_for_target(&registry, &bindings, &project(), &target, &BTreeMap::new())
                    .unwrap(),
                Some(name)
            );
        }
        let bindings = BTreeMap::from([("app".into(), "missing".into())]);
        assert!(
            binding_for_target(&registry, &bindings, &project(), &target, &BTreeMap::new())
                .is_err()
        );
        registry.credentials.get_mut("narrow").unwrap().host = "gitlab.com".into();
        let bindings = BTreeMap::from([("app".into(), "narrow".into())]);
        assert!(
            binding_for_target(&registry, &bindings, &project(), &target, &BTreeMap::new())
                .is_err()
        );
    }

    #[test]
    fn duplicate_remote_assignment_is_ambiguous() {
        let mut project = project();
        project.repos.push(project.repos[0].clone());
        assert!(binding_for_target(
            &AuthStore::default(),
            &BTreeMap::new(),
            &project,
            &remote_target("https://github.com/org/app").unwrap(),
            &BTreeMap::new()
        )
        .is_err());
    }

    fn authed_project() -> KnitProject {
        let mut project = project();
        project.auth = Some(
            serde_json::from_value(serde_json::json!({
                "groups": [{
                    "id": "gh", "name": "GitHub", "provider": "github", "host": "github.com",
                    "repos": ["app"], "tokenTypes": ["classic_pat"]
                }]
            }))
            .unwrap(),
        );
        project
    }

    #[test]
    fn project_auth_validation_rejects_unknown_mismatched_and_ambiguous_refs() {
        let valid = authed_project();
        assert!(validate_project_auth(&valid).is_ok());

        // Unknown repository: a typo, never silently treated as scoped.
        let mut typo = authed_project();
        typo.auth.as_mut().unwrap().groups[0].repos = vec!["appp".into()];
        let error = validate_project_auth(&typo).unwrap_err().to_string();
        assert!(error.contains("unknown repository `appp`"), "{error}");

        // Host mismatch: group host must be the repository's host.
        let mut wrong_host = authed_project();
        wrong_host.auth.as_mut().unwrap().groups[0].host = "ghe.example.com".into();
        assert!(validate_project_auth(&wrong_host)
            .unwrap_err()
            .to_string()
            .contains("is for host"));

        // Provider mismatch on a known forge: the URL is authoritative.
        let mut wrong_provider = authed_project();
        wrong_provider.auth.as_mut().unwrap().groups[0].provider = "gitlab".into();
        wrong_provider.auth.as_mut().unwrap().groups[0].token_types =
            vec!["personal_access_token".into()];
        assert!(validate_project_auth(&wrong_provider)
            .unwrap_err()
            .to_string()
            .contains("declares provider `gitlab`"));

        // A custom host trusts the declared provider (URL says nothing).
        let mut custom = project();
        custom.repos[0].remote = Some("https://git.example.test/org/app.git".into());
        custom.auth = Some(serde_json::from_value(serde_json::json!({
            "groups": [{
                "id": "g", "name": "Self-hosted", "provider": "gitlab", "host": "git.example.test",
                "repos": ["app"], "tokenTypes": ["personal_access_token"]
            }]
        }))
        .unwrap());
        assert!(validate_project_auth(&custom).is_ok());

        // Ambiguous local ids never match any candidate.
        let mut ambiguous = authed_project();
        ambiguous.repos.push(ambiguous.repos[0].clone());
        assert!(validate_project_auth(&ambiguous)
            .unwrap_err()
            .to_string()
            .contains("more than once"));

        // Host comparison is case-insensitive.
        let mut case = authed_project();
        case.auth.as_mut().unwrap().groups[0].host = "GitHub.com".into();
        assert!(validate_project_auth(&case).is_ok());

        // Several groups may share one host as long as repos do not overlap.
        let mut shared = authed_project();
        shared.auth.as_mut().unwrap().groups.push(
            serde_json::from_value(serde_json::json!({
                "id": "gh2", "name": "Second GitHub", "provider": "github", "host": "github.com",
                "repos": ["lib"], "tokenTypes": ["fine_grained_pat"]
            }))
            .unwrap(),
        );
        assert!(validate_project_auth(&shared).is_ok());
    }

    #[test]
    fn pending_membership_references_validate_and_resolve() {
        // Scoped workspace: `lib` was never cloned, but the membership knows
        // it, so the reference validates and a binding can serve its URL.
        let mut project = authed_project();
        project.repos.truncate(1); // only `app` exists locally
        project.auth.as_mut().unwrap().groups[0].repos = vec!["app".into(), "lib".into()];
        let pending = BTreeMap::from([(
            "lib".to_string(),
            "https://github.com/org/lib.git".to_string(),
        )]);
        assert!(validate_project_auth_with_pending(&project, &pending).is_ok());
        // Without the membership record the same reference is a typo.
        assert!(validate_project_auth(&project).is_err());

        // A pending reference must still sit on the group's forge.
        let mut off_host = project.clone();
        off_host.auth.as_mut().unwrap().groups[0].repos = vec!["lib".into()];
        let wrong = BTreeMap::from([(
            "lib".to_string(),
            "https://gitlab.com/org/lib.git".to_string(),
        )]);
        assert!(validate_project_auth_with_pending(&off_host, &wrong).is_err());

        // The resolver serves a pending repository once it has a binding,
        // fails actionably without one, and refuses ambiguity.
        let mut registry = AuthStore::default();
        registry.credentials.insert(
            "work".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "github.com".into(),
                username: None,
                token_env: None,
                token_type: None,
            },
        );
        let target = remote_target("https://github.com/org/lib").unwrap();
        let no_binding = BTreeMap::new();
        assert!(
            binding_for_target(&registry, &no_binding, &project, &target, &pending)
                .unwrap_err()
                .to_string()
                .contains("run `knit auth setup`, then `knit pull`")
        );
        let bindings = BTreeMap::from([("lib".into(), "work".into())]);
        assert_eq!(
            binding_for_target(&registry, &bindings, &project, &target, &pending).unwrap(),
            Some("work")
        );
        let mut two = pending.clone();
        two.insert(
            "lib2".to_string(),
            "https://github.com/org/lib.git".to_string(),
        );
        assert!(
            binding_for_target(&registry, &bindings, &project, &target, &two)
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
    }

    #[test]
    fn token_type_metadata_is_validated_against_the_provider() {
        let mut spec = CredentialSpec {
            provider: "bitbucket".into(),
            host: "bitbucket.org".into(),
            username: None,
            token_env: None,
            token_type: Some("atlassian_api_token".into()),
        };
        assert!(validate_spec(&spec).is_ok());
        spec.token_type = Some("fine_grained_pat".into());
        assert!(validate_spec(&spec).is_err());
        spec.provider = "forgejo".into();
        spec.host = "codeberg.org".into();
        spec.token_type = Some("access_token".into());
        assert!(validate_spec(&spec).is_ok());
        // Forgejo aliases share the same token kinds.
        spec.provider = "codeberg".into();
        assert!(validate_spec(&spec).is_ok());
    }

    #[test]
    fn adhoc_context_keeps_legacy_auth_without_borrowing_another_project() {
        assert_eq!(project_from_bundle(None, None), None);
        assert_eq!(
            project_from_bundle(Some(None), Some("unrelated".into())),
            None
        );
        assert_eq!(
            project_from_bundle(Some(Some("bundle".into())), Some("active".into())),
            Some("bundle".into())
        );
        assert_eq!(
            project_from_bundle(None, Some("active".into())),
            Some("active".into())
        );
    }

    #[test]
    fn project_keys_do_not_collide_across_workspaces() {
        let temporary = std::env::temp_dir().join(format!("knit-auth-{}", std::process::id()));
        for root in [temporary.join("one"), temporary.join("two")] {
            fs::create_dir_all(root.join(".knit/projects")).unwrap();
            fs::write(store::project_path(&root, "same"), "{}").unwrap();
        }
        assert_ne!(
            project_key(&temporary.join("one"), "same").unwrap(),
            project_key(&temporary.join("two"), "same").unwrap()
        );
        fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn private_store_keeps_secrets_out_of_registry_and_restricts_modes() {
        let temporary =
            std::env::temp_dir().join(format!("knit-auth-private-{}", std::process::id()));
        let path = temporary.join("secrets.json");
        write_private(&path, &BTreeMap::from([("work", "SECRET")])).unwrap();
        let read: BTreeMap<String, String> = load_file(&path).unwrap();
        assert_eq!(read["work"], "SECRET");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&temporary).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        fs::remove_dir_all(temporary).unwrap();
    }
}
