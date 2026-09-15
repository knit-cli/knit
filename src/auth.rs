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

/// One exact repository target an explicit clone credential applies to,
/// resolved from the export's clone URL (`remote_target` normalization:
/// lowercase host, `.git`-trimmed path). Matching is exact — host and path —
/// so repositories Git happens to touch outside the cloned selection never
/// receive the credential.
#[derive(Clone, PartialEq, Eq)]
pub struct CloneCredentialTarget {
    pub name: String,
    pub host: String,
    pub path: String,
}

/// The exact-target clone selection, active only while a Drop guard holds it.
/// Activated after the clone scope is resolved and before any Git runs, and
/// restored on every result path; nothing relies on process exit, so library
/// callers are unaffected once the guarded clone section finishes.
static CLONE_CREDENTIALS: Mutex<Option<Vec<CloneCredentialTarget>>> = Mutex::new(None);

/// Activates an exact-target clone credential selection and returns the guard
/// that restores the previous state when dropped.
pub fn activate_clone_credentials(targets: Vec<CloneCredentialTarget>) -> CloneCredentialGuard {
    let previous = {
        let mut active = CLONE_CREDENTIALS
            .lock()
            .expect("clone credential lock poisoned");
        active.replace(targets)
    };
    CloneCredentialGuard { previous }
}

/// Restores the clone credential selection that was active before the guard
/// was created, on every exit path (return, `?`, panic).
pub struct CloneCredentialGuard {
    previous: Option<Vec<CloneCredentialTarget>>,
}

impl Drop for CloneCredentialGuard {
    fn drop(&mut self) {
        *CLONE_CREDENTIALS
            .lock()
            .expect("clone credential lock poisoned") = self.previous.take();
    }
}

/// The active exact-target clone selection, if a guarded clone section set one.
pub fn clone_credentials() -> Option<Vec<CloneCredentialTarget>> {
    CLONE_CREDENTIALS
        .lock()
        .expect("clone credential lock poisoned")
        .clone()
}

/// The credential selected for exactly this forge target, if any.
pub fn clone_credential_for(host: &str, path: &str) -> Option<String> {
    clone_credentials()?.into_iter().find_map(|target| {
        (target.host.eq_ignore_ascii_case(host) && target.path == path).then_some(target.name)
    })
}

/// Validate `--credential` names before a clone changes anything: each name
/// must exist, its token must be resolvable now, and no two names may claim
/// the same host. Returns the deduplicated (name, host) selection. With no
/// names, the private credential registry is not read at all, so ordinary
/// clones are unaffected.
pub fn validate_clone_credentials(names: &[String]) -> Result<Vec<(String, String)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let registry = load()?;
    let selection = validated_clone_selection(&registry, names)?;
    for (name, _) in &selection {
        // Fail before any clone starts when the token is determinably
        // missing (unsaved token, unset environment reference). A token the
        // forge refuses surfaces at clone time with the credential named.
        credential(name)
            .with_context(|| format!("Credential `{name}` cannot be used for this clone"))?;
    }
    Ok(selection)
}

fn validated_clone_selection(
    registry: &AuthStore,
    names: &[String],
) -> Result<Vec<(String, String)>> {
    let mut selection: Vec<(String, String)> = Vec::new();
    for name in names {
        if selection.iter().any(|(selected, _)| selected == name) {
            continue;
        }
        validate_name(name)?;
        let spec = registry.credentials.get(name).with_context(|| {
            let available: Vec<&str> = registry.credentials.keys().map(String::as_str).collect();
            format!(
                "Credential `{name}` is not configured; run `knit auth add` first. {}",
                if available.is_empty() {
                    "No credentials are saved on this machine.".to_string()
                } else {
                    format!("Saved credentials: {}.", available.join(", "))
                }
            )
        })?;
        validate_spec(spec)?;
        let host = spec.host.to_ascii_lowercase();
        if let Some((other, _)) = selection
            .iter()
            .find(|(_, selected)| selected.eq_ignore_ascii_case(&host))
        {
            bail!(
                "Credentials `{other}` and `{name}` are both for {host}; a clone can select only one credential per host"
            );
        }
        selection.push((name.clone(), host));
    }
    Ok(selection)
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

fn validate_store(value: &AuthStore) -> Result<()> {
    for (name, spec) in &value.credentials {
        validate_name(name)?;
        validate_spec(spec)?;
    }
    Ok(())
}

pub fn save(value: &AuthStore) -> Result<()> {
    validate_store(value)?;
    write_private(&personal_path("forge-auth.json")?, value)
}

/// Save one credential and its token-source transition while the caller holds
/// the auth lock. Validate and stage everything before changing either file.
/// The secret is replaced last: a failed save leaves the old token untouched.
pub fn save_credential(value: &AuthStore, name: &str, token: Option<&str>) -> Result<()> {
    validate_store(value)?;
    validate_name(name)?;
    let spec = value
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
    let registry = prepare_private(&registry_path, value)?;
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
    let Some(name) = binding_for_target(&registry, bindings, &project, &target)? else {
        return Ok(None);
    };
    // binding_for_target validates the credential metadata and exact host.
    Ok(Some(registry.credentials[name].provider.clone()))
}

pub fn resolve(cwd: &Path, remote: Option<&str>) -> Result<Option<ResolvedCredential>> {
    if let Some(selection) = clone_credentials() {
        return resolve_clone_selection(&selection, cwd, remote);
    }
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
    let Some(name) = binding_for_target(&registry, bindings, &project, &target)? else {
        return Ok(None);
    };
    let resolved = credential(name)?;
    if resolved.host != target.0 {
        bail!("Assigned credential host does not match the repository host");
    }
    Ok(Some(resolved))
}

/// Resolve one Git remote against the active clone selection: a selected
/// credential is used only when its exact (host, path) target matches the
/// remote; every other repository keeps existing ambient Git authentication.
/// Project assignments from a surrounding workspace are never borrowed while
/// a clone selection is active.
fn resolve_clone_selection(
    selection: &[CloneCredentialTarget],
    cwd: &Path,
    remote: Option<&str>,
) -> Result<Option<ResolvedCredential>> {
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
    if is_local_remote(remote) {
        return Ok(None);
    }
    let (host, path) = remote_target(remote)?;
    let Some(name) = clone_selection_match(selection, &host, &path) else {
        return Ok(None);
    };
    // credential() fails closed when the credential or its token disappeared
    // since validation; an explicit selection never falls back silently.
    let resolved = credential(name)?;
    if resolved.host != host {
        bail!("Selected credential host does not match the repository host");
    }
    Ok(Some(resolved))
}

/// The credential selected for exactly this target, if the selection has one.
fn clone_selection_match<'a>(
    selection: &'a [CloneCredentialTarget],
    host: &str,
    path: &str,
) -> Option<&'a str> {
    selection
        .iter()
        .find(|target| target.host.eq_ignore_ascii_case(host) && target.path == path)
        .map(|target| target.name.as_str())
}

fn binding_for_target<'a>(
    registry: &'a AuthStore,
    bindings: &'a BTreeMap<String, String>,
    project: &KnitProject,
    target: &(String, String),
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
        validate_name(name)?;
        let spec = registry
            .credentials
            .get(name)
            .with_context(|| format!("Assigned credential `{name}` is not configured"))?;
        validate_spec(spec)?;
        if !spec.host.eq_ignore_ascii_case(&target.0) {
            bail!("Assigned credential host does not match the repository host");
        }
        return Ok(Some(name));
    }
    bail!("Remote is not a configured repository in this project; refusing to select a forge credential")
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
            },
        );
        let bindings = BTreeMap::from([("app".into(), "work".into())]);
        assert_eq!(
            binding_for_target(
                &registry,
                &bindings,
                &project(),
                &remote_target("https://github.com/org/app").unwrap()
            )
            .unwrap(),
            Some("work")
        );
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &remote_target("https://github.com/org/lib").unwrap()
        )
        .is_err());
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &remote_target("https://gitlab.com/other/app").unwrap()
        )
        .is_err());
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &remote_target("https://github.com/other/app").unwrap()
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
        };
        let mut registry = AuthStore::default();
        registry.credentials.insert("narrow".into(), spec.clone());
        registry.credentials.insert("classic".into(), spec);
        let target = remote_target("https://github.com/org/app").unwrap();
        for name in ["narrow", "classic"] {
            let bindings = BTreeMap::from([("app".into(), name.into())]);
            assert_eq!(
                binding_for_target(&registry, &bindings, &project(), &target).unwrap(),
                Some(name)
            );
        }
        let bindings = BTreeMap::from([("app".into(), "missing".into())]);
        assert!(binding_for_target(&registry, &bindings, &project(), &target).is_err());
        registry.credentials.get_mut("narrow").unwrap().host = "gitlab.com".into();
        let bindings = BTreeMap::from([("app".into(), "narrow".into())]);
        assert!(binding_for_target(&registry, &bindings, &project(), &target).is_err());
    }

    #[test]
    fn duplicate_remote_assignment_is_ambiguous() {
        let mut project = project();
        project.repos.push(project.repos[0].clone());
        assert!(binding_for_target(
            &AuthStore::default(),
            &BTreeMap::new(),
            &project,
            &remote_target("https://github.com/org/app").unwrap()
        )
        .is_err());
    }

    #[test]
    fn clone_selection_rejects_unknown_and_duplicate_hosts() {
        let mut registry = AuthStore::default();
        registry.credentials.insert(
            "work".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "GitHub.com".into(),
                username: None,
                token_env: None,
            },
        );
        registry.credentials.insert(
            "legacy".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "github.com".into(),
                username: None,
                token_env: None,
            },
        );
        registry.credentials.insert(
            "cloud".into(),
            CredentialSpec {
                provider: "bitbucket".into(),
                host: "bitbucket.org".into(),
                username: None,
                token_env: None,
            },
        );

        // Repeating one name is idempotent; different providers coexist.
        assert_eq!(
            validated_clone_selection(
                &registry,
                &["work".to_string(), "work".to_string(), "cloud".to_string()]
            )
            .unwrap(),
            vec![
                ("work".to_string(), "github.com".to_string()),
                ("cloud".to_string(), "bitbucket.org".to_string())
            ]
        );

        let error = validated_clone_selection(&registry, &["missing".to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("`missing` is not configured"), "{error}");
        assert!(
            error.contains("Saved credentials: cloud, legacy, work"),
            "{error}"
        );

        let error =
            validated_clone_selection(&registry, &["work".to_string(), "legacy".to_string()])
                .unwrap_err()
                .to_string();
        assert!(
            error.contains("`work` and `legacy` are both for github.com"),
            "{error}"
        );

        assert_eq!(
            validated_clone_selection(&registry, &[]).unwrap(),
            Vec::<(String, String)>::new()
        );
    }

    #[test]
    fn clone_selection_matches_only_the_exact_target() {
        let targets = |paths: &[(&str, &str)]| {
            paths
                .iter()
                .map(|(name, path)| CloneCredentialTarget {
                    name: (*name).to_string(),
                    host: "github.com".to_string(),
                    path: (*path).to_string(),
                })
                .collect::<Vec<_>>()
        };
        let selection = targets(&[("work", "acme/backend"), ("other", "other/repo")]);
        assert_eq!(
            clone_selection_match(&selection, "github.com", "acme/backend"),
            Some("work")
        );
        // Same host, different path: not covered.
        assert_eq!(
            clone_selection_match(&selection, "github.com", "acme/another"),
            None
        );
        // Path is compared with its normalized (`.git`-trimmed) form only;
        // `.git` variants never widen the scope.
        assert_eq!(
            clone_selection_match(&selection, "github.com", "acme/backend.git"),
            None
        );
        assert_eq!(
            clone_selection_match(&selection, "GITHUB.COM", "acme/backend"),
            Some("work")
        );
        assert_eq!(
            clone_selection_match(&selection, "bitbucket.org", "acme/backend"),
            None
        );
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
