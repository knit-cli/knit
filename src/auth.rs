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
///
/// `root` pins the selection to the clone's target workspace: a remote the
/// selection does not cover resolves its project assignments there and only
/// there, so a surrounding workspace's assignments are unreachable even if a
/// child Git process runs with a different cwd.
#[derive(Clone)]
struct CloneCredentialScope {
    root: Option<PathBuf>,
    targets: Vec<CloneCredentialTarget>,
}

static CLONE_CREDENTIALS: Mutex<Option<CloneCredentialScope>> = Mutex::new(None);

/// Activates an exact-target clone credential selection and returns the guard
/// that restores the previous state when dropped. Project-assignment
/// fallbacks continue to resolve from the caller's cwd.
pub fn activate_clone_credentials(targets: Vec<CloneCredentialTarget>) -> CloneCredentialGuard {
    activate_scope(CloneCredentialScope {
        root: None,
        targets,
    })
}

/// Activates an exact-target clone credential selection pinned to one clone
/// target root: uncovered remotes resolve assignments only inside that root.
pub fn activate_clone_credentials_at(
    root: PathBuf,
    targets: Vec<CloneCredentialTarget>,
) -> CloneCredentialGuard {
    activate_scope(CloneCredentialScope {
        root: Some(root),
        targets,
    })
}

/// Activates the isolation without any selected targets — a grouped or
/// ambient clone section whose project-assignment fallbacks must stay inside
/// the clone target root.
pub fn activate_clone_root(root: PathBuf) -> CloneCredentialGuard {
    activate_scope(CloneCredentialScope {
        root: Some(root),
        targets: Vec::new(),
    })
}

fn activate_scope(scope: CloneCredentialScope) -> CloneCredentialGuard {
    let previous = CLONE_CREDENTIALS
        .lock()
        .expect("clone credential lock poisoned")
        .replace(scope);
    CloneCredentialGuard { previous }
}

/// Restores the clone credential selection that was active before the guard
/// was created, on every exit path (return, `?`, panic).
pub struct CloneCredentialGuard {
    previous: Option<CloneCredentialScope>,
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
        .as_ref()
        .map(|scope| scope.targets.clone())
}

/// The root a guarded clone section pinned its assignment fallbacks to.
pub fn clone_credential_root() -> Option<PathBuf> {
    CLONE_CREDENTIALS
        .lock()
        .expect("clone credential lock poisoned")
        .as_ref()
        .and_then(|scope| scope.root.clone())
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
    /// Canonical project artifact path -> repository id -> normalized remote
    /// target ("host/path") that already works with ambient Git access: it
    /// was cloned or verified without any Knit credential (public HTTPS, or
    /// an SSH key). Once a project has assignments the resolver refuses to
    /// send its other forge repositories through unknown ambient credentials
    /// — this allowance is the recorded exception, and it is bound to the
    /// exact remote: a URL change revokes it until the new remote is
    /// verified again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ambient: BTreeMap<String, BTreeMap<String, String>>,
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

/// Record that one repository already works with ambient Git access, bound
/// to the exact normalized remote target. Best-effort and caller-friendly:
/// an unparseable remote is skipped, not an error.
pub fn record_ambient_access(
    root: &Path,
    project: &str,
    repo_id: &str,
    remote: &str,
) -> Result<()> {
    let target = remote_target(remote).ok();
    let _lock = lock()?;
    let mut store = load()?;
    let key = project_key(root, project)?;
    let mut projects = store.ambient.remove(&key).unwrap_or_default();
    match target {
        Some((host, path)) => {
            projects.insert(repo_id.to_owned(), format!("{host}/{path}"));
        }
        None => {
            projects.remove(repo_id);
        }
    }
    if projects.is_empty() {
        store.ambient.remove(&key);
    } else {
        store.ambient.insert(key, projects);
    }
    save(&store)
}

/// The ambient allowance for this project, for the resolver's strict gate.
fn ambient_for<'a>(store: &'a AuthStore, key: &str) -> &'a BTreeMap<String, String> {
    store.ambient.get(key).unwrap_or(&EMPTY_MAP)
}

/// Whether this repository's recorded ambient allowance still matches the
/// exact remote target being resolved. The allowance is bound to one
/// host/path, so a URL change revokes it until the new remote is verified.
pub(crate) fn ambient_allows(
    ambient: &BTreeMap<String, String>,
    repo_id: &str,
    host: &str,
    path: &str,
) -> bool {
    ambient.get(repo_id).map(String::as_str) == Some(format!("{host}/{path}").as_str())
}

static EMPTY_MAP: BTreeMap<String, String> = BTreeMap::new();

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
    let ambient = ambient_for(&registry, &key);
    let Some(name) = binding_for_target(&registry, bindings, &project, &target, &pending, ambient)?
    else {
        return Ok(None);
    };
    // binding_for_target validates the credential metadata and exact host.
    Ok(Some(registry.credentials[name].provider.clone()))
}

pub fn resolve(cwd: &Path, remote: Option<&str>) -> Result<Option<ResolvedCredential>> {
    // An exact-target clone selection authenticates only the repositories it
    // names. A remote it does not cover is not forced to ambient credentials:
    // the selection falls through to the clone workspace's project
    // assignments, so grouped mappings (written by guided setup before the
    // guard activated) keep serving their repositories while the selection is
    // active. The fallback is pinned to the clone's target root when the
    // guard carries one, so the surrounding workspace is never borrowed.
    if clone_credentials().is_some() {
        let root = clone_credential_root();
        if let Some(selected) =
            resolve_clone_selection(&clone_credentials().expect("just checked"), cwd, remote)?
        {
            return Ok(Some(selected));
        }
        if let Some(root) = root.as_deref() {
            return resolve_clone_root_assignment(root, remote);
        }
    }
    resolve_project_assignment(cwd, remote)
}

/// Resolve a remote against this workspace's project assignments (and the
/// recorded ambient allowances), without any clone selection involved.
fn resolve_project_assignment(
    cwd: &Path,
    remote: Option<&str>,
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
    resolve_remote_in_project(&registry, &root, &project, bindings, cwd, remote)
}

/// Resolve a remote inside a guarded clone section's pinned root. The clone
/// workspace's own `activeProject` is authoritative here: the surrounding
/// process's project override or bundle selection (an outer bundle worktree
/// the clone was invoked from) must not redirect assignment fallbacks into a
/// different project's bindings.
fn resolve_clone_root_assignment(
    root: &Path,
    remote: Option<&str>,
) -> Result<Option<ResolvedCredential>> {
    let registry = load()?;
    if registry.projects.values().all(BTreeMap::is_empty) {
        return Ok(None);
    }
    let config = store::load_config(root)?;
    let Some(project_id) = config.active_project else {
        return Ok(None);
    };
    validate_name(&project_id).context("Invalid project id")?;
    let project_path = store::project_path(root, &project_id);
    if !project_path.exists() {
        return Ok(None);
    }
    let project: KnitProject = store::read_json(&project_path)?;
    let key = project_key(root, &project_id)?;
    let Some(bindings) = registry.projects.get(&key).filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    resolve_remote_in_project(&registry, root, &project, bindings, root, remote)
}

/// The shared assignment resolution once the workspace root, project, and
/// bindings are known: remote normalization, the strict gate, and the
/// credential lookup. `origin_cwd` is where `origin` is read from when no
/// explicit remote is given (the caller's cwd, or the pinned root).
fn resolve_remote_in_project(
    registry: &AuthStore,
    root: &Path,
    project: &KnitProject,
    bindings: &BTreeMap<String, String>,
    origin_cwd: &Path,
    remote: Option<&str>,
) -> Result<Option<ResolvedCredential>> {
    let origin;
    let remote = match remote {
        Some(remote) => remote,
        None => {
            let output = Command::new("git")
                .args(["remote", "get-url", "origin"])
                .current_dir(origin_cwd)
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
    let pending = load_known_pending_repos(root, &project.id);
    let ambient = ambient_for(registry, &project_key(root, &project.id)?);
    let Some(name) = binding_for_target(registry, bindings, project, &target, &pending, ambient)?
    else {
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
/// remote; every other repository falls back to the caller's normal
/// resolution (project assignments, then ambient Git authentication).
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

/// Whether a declared auth group claims this repository id (membership can
/// also be a pending repository the workspace has not materialized). A
/// declared member must authenticate through its group's credential: an
/// ambient allowance recorded while it was still ungrouped is stale the
/// moment the group claims it, and never satisfies the gate.
fn declared_group_member(project: &KnitProject, repo_id: &str) -> bool {
    project.auth.as_ref().is_some_and(|auth| {
        auth.groups
            .iter()
            .any(|group| group.repos.iter().any(|repo| repo == repo_id))
    })
}

fn binding_for_target<'a>(
    registry: &'a AuthStore,
    bindings: &'a BTreeMap<String, String>,
    project: &KnitProject,
    target: &(String, String),
    pending: &BTreeMap<String, String>,
    ambient: &BTreeMap<String, String>,
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
        if let Some(name) = bindings.get(&repo.id) {
            return validated_binding(registry, name, &target.0);
        }
        // Recorded ambient access — a public HTTPS or SSH repository this
        // project already cloned or verified without a credential — keeps
        // working after assignments exist, but only for the exact remote it
        // was recorded for, and never once a declared group claims the
        // repository: group members need the group's credential.
        if !declared_group_member(project, &repo.id)
            && ambient_allows(ambient, &repo.id, &target.0, &target.1)
        {
            return Ok(None);
        }
        bail!(
            "Project `{}` repository `{}` has no assigned credential; run `knit auth setup`",
            project.id,
            repo.id
        );
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
        // The same precedence as a materialized repository: an explicit
        // binding wins, an exact recorded ambient allowance lets the remote
        // through without one — unless a declared group claims the pending
        // repository, in which case the allowance is stale — and anything
        // else stays strict.
        if let Some(name) = bindings.get(repo_id.as_str()) {
            return validated_binding(registry, name, &target.0);
        }
        if !declared_group_member(project, repo_id)
            && ambient_allows(ambient, repo_id, &target.0, &target.1)
        {
            return Ok(None);
        }
        bail!(
            "Project `{}` repository `{repo_id}` has no assigned credential yet; run `knit auth setup`, then `knit pull` to clone it",
            project.id
        );
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
    fn ambient_allowance_lets_recorded_remotes_through_the_strict_gate() {
        // The project has one assignment (`app` -> work), so the strict gate
        // applies to `lib`. A recorded ambient allowance bound to the exact
        // remote lets `lib` resolve to ambient; anything else still fails.
        let mut registry = AuthStore::default();
        registry.credentials.insert(
            "work".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "github.com".into(),
                username: None,
                token_type: None,
                token_env: None,
            },
        );
        let bindings = BTreeMap::from([("app".to_string(), "work".to_string())]);
        let target = remote_target("https://github.com/org/lib").unwrap();
        let ambient = BTreeMap::from([("lib".to_string(), "github.com/org/lib".to_string())]);
        // Exact recorded remote: ambient access, no credential.
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &target,
            &BTreeMap::new(),
            &ambient
        )
        .unwrap()
        .is_none());
        // A different repository with no allowance stays strict.
        let app_target = remote_target("git@github.com:org/app.git").unwrap();
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &app_target,
            &BTreeMap::new(),
            &ambient
        )
        .unwrap()
        .is_some());
        // A URL change (renamed repository) revokes the allowance: the
        // recorded target no longer matches, so the gate fails closed.
        let moved = remote_target("https://github.com/org/lib-renamed").unwrap();
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &moved,
            &BTreeMap::new(),
            &ambient
        )
        .is_err());
    }

    #[test]
    fn pending_repositories_get_the_same_ambient_allowance_as_materialized_ones() {
        // A repository the membership knows but this workspace has not
        // materialized (the pending sidecar) follows the same precedence as a
        // tracked repo: binding first, exact ambient allowance second, strict
        // error otherwise.
        let mut registry = AuthStore::default();
        registry.credentials.insert(
            "work".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "github.com".into(),
                username: None,
                token_type: None,
                token_env: None,
            },
        );
        let bindings = BTreeMap::from([("app".to_string(), "work".to_string())]);
        let target = remote_target("https://github.com/org/pend.git").unwrap();
        let pending = BTreeMap::from([(
            "pend".to_string(),
            "https://github.com/org/pend.git".to_string(),
        )]);
        let ambient = BTreeMap::from([("pend".to_string(), "github.com/org/pend".to_string())]);
        // Ambient allowance, no binding: through, no credential selected.
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &target,
            &pending,
            &ambient
        )
        .unwrap()
        .is_none());
        // An existing binding wins over the recorded allowance.
        let mut bound = bindings.clone();
        bound.insert("pend".to_string(), "work".to_string());
        assert_eq!(
            binding_for_target(&registry, &bound, &project(), &target, &pending, &ambient).unwrap(),
            Some("work")
        );
        // A URL change revokes the allowance: the pending remote moved, the
        // allowance still names the old target, so the gate fails closed.
        let moved = remote_target("https://github.com/org/pend-renamed.git").unwrap();
        let pending_moved = BTreeMap::from([(
            "pend".to_string(),
            "https://github.com/org/pend-renamed.git".to_string(),
        )]);
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &moved,
            &pending_moved,
            &ambient
        )
        .is_err());
    }

    #[test]
    fn declared_group_members_never_accept_stale_ambient_allowances() {
        // `lib` was public and cloned ambient (allowance recorded); the
        // remote later added it to a declared group. The allowance is stale:
        // the gate must demand the group's credential, in the tracked and
        // the pending shape alike, while an existing binding keeps winning.
        let grouped = serde_json::from_value::<KnitProject>(serde_json::json!({
            "schemaVersion": "1", "kind": "KnitProject", "id": "one", "createdAt": "", "updatedAt": "",
            "repos": [{"id":"app", "path":"app", "remote":"git@github.com:org/app.git", "baseBranch":"main"},
                      {"id":"lib", "path":"lib", "remote":"https://github.com/org/lib", "baseBranch":"main"}],
            "auth": {"groups": [{
                "id": "lib-group", "name": "Library", "provider": "github",
                "host": "github.com", "repos": ["lib"], "tokenTypes": ["fine_grained_pat"]
            }]}
        }))
        .unwrap();
        let mut registry = AuthStore::default();
        registry.credentials.insert(
            "work".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "github.com".into(),
                username: None,
                token_type: None,
                token_env: None,
            },
        );
        let bindings = BTreeMap::from([("app".to_string(), "work".to_string())]);
        let target = remote_target("https://github.com/org/lib").unwrap();
        let ambient = BTreeMap::from([("lib".to_string(), "github.com/org/lib".to_string())]);
        // Tracked member: the exact-match allowance no longer applies.
        let error = binding_for_target(
            &registry,
            &bindings,
            &grouped,
            &target,
            &BTreeMap::new(),
            &ambient,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("no assigned credential"),
            "{error}"
        );
        // An explicit binding for the member still wins over the stale
        // allowance and over the strict error.
        let mut bound = bindings.clone();
        bound.insert("lib".to_string(), "work".to_string());
        assert_eq!(
            binding_for_target(
                &registry,
                &bound,
                &grouped,
                &target,
                &BTreeMap::new(),
                &ambient
            )
            .unwrap(),
            Some("work")
        );
        // Pending member: same rule. `pend` sits only in the membership
        // sidecar but is declared in the group.
        let pending = BTreeMap::from([(
            "pend".to_string(),
            "https://github.com/org/pend.git".to_string(),
        )]);
        let pending_target = remote_target("https://github.com/org/pend.git").unwrap();
        let mut grouped_pending = grouped.clone();
        grouped_pending.auth.as_mut().unwrap().groups[0]
            .repos
            .push("pend".into());
        let pending_ambient =
            BTreeMap::from([("pend".to_string(), "github.com/org/pend".to_string())]);
        assert!(
            binding_for_target(
                &registry,
                &bindings,
                &grouped_pending,
                &pending_target,
                &pending,
                &pending_ambient
            )
            .is_err(),
            "declared pending member must not resolve through a stale allowance"
        );
        // The same pending repository without group coverage keeps its
        // allowance.
        assert!(binding_for_target(
            &registry,
            &bindings,
            &grouped,
            &pending_target,
            &pending,
            &pending_ambient
        )
        .unwrap()
        .is_none());
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
                &BTreeMap::new(),
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
            &BTreeMap::new(),
            &BTreeMap::new()
        )
        .is_err());
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &remote_target("https://gitlab.com/other/app").unwrap(),
            &BTreeMap::new(),
            &BTreeMap::new()
        )
        .is_err());
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &remote_target("https://github.com/other/app").unwrap(),
            &BTreeMap::new(),
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
                binding_for_target(
                    &registry,
                    &bindings,
                    &project(),
                    &target,
                    &BTreeMap::new(),
                    &BTreeMap::new()
                )
                .unwrap(),
                Some(name)
            );
        }
        let bindings = BTreeMap::from([("app".into(), "missing".into())]);
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &target,
            &BTreeMap::new(),
            &BTreeMap::new()
        )
        .is_err());
        registry.credentials.get_mut("narrow").unwrap().host = "gitlab.com".into();
        let bindings = BTreeMap::from([("app".into(), "narrow".into())]);
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project(),
            &target,
            &BTreeMap::new(),
            &BTreeMap::new()
        )
        .is_err());
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
            &BTreeMap::new(),
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
        assert!(binding_for_target(
            &registry,
            &no_binding,
            &project,
            &target,
            &pending,
            &BTreeMap::new()
        )
        .unwrap_err()
        .to_string()
        .contains("run `knit auth setup`, then `knit pull`"));
        let bindings = BTreeMap::from([("lib".into(), "work".into())]);
        assert_eq!(
            binding_for_target(
                &registry,
                &bindings,
                &project,
                &target,
                &pending,
                &BTreeMap::new()
            )
            .unwrap(),
            Some("work")
        );
        let mut two = pending.clone();
        two.insert(
            "lib2".to_string(),
            "https://github.com/org/lib.git".to_string(),
        );
        assert!(binding_for_target(
            &registry,
            &bindings,
            &project,
            &target,
            &two,
            &BTreeMap::new()
        )
        .unwrap_err()
        .to_string()
        .contains("ambiguous"));
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
    fn clone_selection_rejects_unknown_and_duplicate_hosts() {
        let mut registry = AuthStore::default();
        registry.credentials.insert(
            "work".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "GitHub.com".into(),
                username: None,
                token_type: None,
                token_env: None,
            },
        );
        registry.credentials.insert(
            "legacy".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "github.com".into(),
                username: None,
                token_type: None,
                token_env: None,
            },
        );
        registry.credentials.insert(
            "cloud".into(),
            CredentialSpec {
                provider: "bitbucket".into(),
                host: "bitbucket.org".into(),
                username: None,
                token_type: None,
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

    /// Child body for the clone-root resolver test; runs with an isolated
    /// KNIT_HOME and a cwd inside the outer workspace.
    #[test]
    fn clone_root_resolver_child() {
        let Ok(outer) = std::env::var("KNIT_CLONE_ROOT_OUTER") else {
            return;
        };
        let outer = PathBuf::from(outer);
        let clone_root = PathBuf::from(std::env::var("KNIT_CLONE_ROOT_TARGET").unwrap());
        let key_outer = project_key(&outer, "outerproj").unwrap();
        let key_clone = project_key(&clone_root, "cloneproj").unwrap();
        let mut store = AuthStore::default();
        store.credentials.insert(
            "work".into(),
            CredentialSpec {
                provider: "github".into(),
                host: "github.com".into(),
                username: None,
                token_type: None,
                token_env: None,
            },
        );
        save_credential(&store, "work", Some("synthetic-child-token")).unwrap();
        store.projects.insert(
            key_outer,
            BTreeMap::from([("o-r".to_string(), "work".to_string())]),
        );
        store.projects.insert(
            key_clone,
            BTreeMap::from([("c-r".to_string(), "work".to_string())]),
        );
        store.ambient.clear();
        save(&store).unwrap();

        // The outer process context selects the outer project (as a command
        // run from a parent bundle worktree would). The guarded clone section
        // pins assignments to the clone root: its project must answer, not
        // the override — the outer project has no `c-r` repository at all.
        set_project_override(Some("outerproj".into()));
        let guard = activate_clone_root(clone_root.clone());
        let resolved = resolve(&outer, Some("https://github.com/org/c.git"))
            .expect("clone root assignment resolution failed");
        assert_eq!(
            resolved.as_ref().map(|credential| credential.name.as_str()),
            Some("work"),
            "clone root's own project must resolve the clone repositories"
        );
        drop(guard);
        set_project_override(None);

        // Normal (non-clone) context is unchanged: the outer project's own
        // repository resolves through the regular path.
        let resolved = resolve(&outer, Some("https://github.com/org/o.git"))
            .expect("outer assignment resolution failed")
            .expect("outer repository should resolve");
        assert_eq!(resolved.name, "work");
    }

    #[test]
    fn clone_root_assignments_ignore_outer_bundle_context() {
        let temporary =
            std::env::temp_dir().join(format!("knit-clone-root-{}", std::process::id()));
        let outer = temporary.join("outer");
        let clone_root = temporary.join("clonetarget");
        for (root, project_id, repo_id, remote) in [
            (&outer, "outerproj", "o-r", "https://github.com/org/o.git"),
            (
                &clone_root,
                "cloneproj",
                "c-r",
                "https://github.com/org/c.git",
            ),
        ] {
            fs::create_dir_all(root.join(".knit/projects")).unwrap();
            fs::write(
                root.join(".knit/config.json"),
                format!(r#"{{"schemaVersion":"0.1","activeProject":"{project_id}"}}"#),
            )
            .unwrap();
            fs::write(
                root.join(format!(".knit/projects/{project_id}.project.json")),
                serde_json::json!({
                    "schemaVersion": "1", "kind": "KnitProject", "id": project_id,
                    "createdAt": "", "updatedAt": "",
                    "repos": [{"id": repo_id, "path": repo_id, "remote": remote,
                               "baseBranch": "main"}]
                })
                .to_string(),
            )
            .unwrap();
        }
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "auth::tests::clone_root_resolver_child",
                "--nocapture",
            ])
            .current_dir(&outer)
            .env("KNIT_CLONE_ROOT_OUTER", &outer)
            .env("KNIT_CLONE_ROOT_TARGET", &clone_root)
            .env("KNIT_HOME", temporary.join("home"))
            .env_remove("KNIT_BUNDLE")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        fs::remove_dir_all(temporary).unwrap();
    }
}
