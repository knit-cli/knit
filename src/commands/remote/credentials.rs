//! Forge credential plumbing. The public helper endpoint returns a credential
//! only on Git's credential-protocol stdout. Clone/fetch retries install that
//! helper for one Git process, so raw forge secrets never cross argv,
//! environment variables, Git config, or disk.

use super::client::request;
use super::clone::export_repo_local_id;
use super::{RemoteExportRepository, RemoteProjectExport};
use crate::git::git_output;
use crate::model::KnitRemote;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Appended to a repo's failure when the server has no credential to vend.
pub(super) const NO_ACCESS_HINT: &str =
    "no HTTPS git access: connect your forge account on the remote or configure SSH credentials";

/// Marker recorded in `--json` repo entries for repos that rode a vended
/// credential; part of the machine-readable contract with ivaldi.
pub(super) const VENDED_CREDENTIAL_LABEL: &str = "remote-vended";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VendedCredential {
    pub(crate) username: String,
    pub(crate) password: String,
}

/// Result of asking the server for a credential: either one to retry with, or
/// an authoritative absence/unsupported host response.
pub(crate) enum VendAttempt {
    Credential(VendedCredential),
    NoCredential,
}

pub(super) struct BrokeredCredentialHelper {
    remote_name: String,
    host: String,
}

struct VendRepo {
    host: Option<String>,
    visibility: Option<String>,
}

/// Everything needed to attach the named remote helper to repositories in one
/// export: the arbitrary remote name and each repository's exact HTTPS host.
pub(super) struct GitCredentialSource {
    remote_name: String,
    repos: BTreeMap<String, VendRepo>,
}

impl GitCredentialSource {
    pub(super) fn from_export(
        remote_name: &str,
        remote: &KnitRemote,
        token: &str,
        export: &RemoteProjectExport,
    ) -> Self {
        let connected_hosts = connected_forge_hosts(remote, token);
        let repos = export
            .repositories
            .iter()
            .map(|repository: &RemoteExportRepository| {
                (
                    export_repo_local_id(repository),
                    VendRepo {
                        host: repository
                            .remote_url
                            .as_deref()
                            .and_then(https_host_from_url)
                            .filter(|host| connected_hosts.contains(host)),
                        visibility: repository.visibility.clone(),
                    },
                )
            })
            .collect();
        Self {
            remote_name: remote_name.to_string(),
            repos,
        }
    }

    /// A failed git op should attempt a vend only for a known, addressable,
    /// non-public repo. Unknown visibility (older exports) still attempts the
    /// vend; an explicitly public repo never does — its failure cannot be an
    /// auth problem the server could fix.
    fn should_vend(&self, local_id: &str) -> bool {
        let Some(repo) = self.repos.get(local_id) else {
            return false;
        };
        if repo.visibility.as_deref() == Some("public") {
            return false;
        }
        repo.host.is_some()
    }

    fn helper(&self, local_id: &str) -> Result<BrokeredCredentialHelper> {
        let repo = self
            .repos
            .get(local_id)
            .with_context(|| format!("{local_id}: not part of the remote export"))?;
        let host = repo
            .host
            .as_deref()
            .with_context(|| format!("{local_id}: repository has no supported HTTPS host"))?;
        // Transparency without resolving the raw credential in this process.
        crate::human!(
            "{}: {}",
            crate::output::repo(local_id),
            crate::output::muted("using the configured remote credential helper")
        );
        Ok(BrokeredCredentialHelper {
            remote_name: self.remote_name.clone(),
            host: host.to_string(),
        })
    }
}

fn connected_forge_hosts(remote: &KnitRemote, token: &str) -> BTreeSet<String> {
    #[derive(Deserialize)]
    struct Descriptor {
        connected: bool,
        hosts: Vec<String>,
    }

    #[derive(Deserialize)]
    struct Envelope {
        data: Vec<Descriptor>,
    }

    let Ok(response) = request(remote, token, "GET", "/me/forge-credentials", None) else {
        return BTreeSet::new();
    };
    if !(200..300).contains(&response.status) {
        return BTreeSet::new();
    }
    serde_json::from_str::<Envelope>(&response.body)
        .map(|envelope| {
            envelope
                .data
                .into_iter()
                .filter(|descriptor| descriptor.connected)
                .flat_map(|descriptor| descriptor.hosts)
                .filter_map(|host| normalize_git_target("https", &host))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Serialize)]
struct ForgeCredentialRequest<'a> {
    protocol: &'a str,
    host: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
}

/// Ask a named KnitHub-compatible remote for the token subject's forge
/// credential. Unsupported hosts and missing connections are soft misses so
/// Git can continue through any other configured credential helpers.
pub(crate) fn request_forge_credential(
    remote: &KnitRemote,
    token: &str,
    protocol: &str,
    host: &str,
    path: Option<&str>,
) -> Result<VendAttempt> {
    let payload = serde_json::to_value(ForgeCredentialRequest {
        protocol,
        host,
        path,
    })?;
    let response = request(
        remote,
        token,
        "POST",
        "/me/forge-credentials/git",
        Some(&payload),
    )?;
    if matches!(response.status, 404 | 409) {
        return Ok(VendAttempt::NoCredential);
    }
    if !(200..300).contains(&response.status) {
        bail!(
            "Sync remote returned HTTP {}: {}",
            response.status,
            response.body.trim()
        );
    }
    #[derive(Deserialize)]
    struct Envelope {
        data: VendedCredential,
    }
    let envelope: Envelope = serde_json::from_str(&response.body)
        .context("failed to parse forge credential response")?;
    Ok(VendAttempt::Credential(envelope.data))
}

pub(crate) fn normalize_git_target(protocol: &str, host: &str) -> Option<String> {
    if protocol != "https" || host.trim() != host || !host.is_ascii() {
        return None;
    }
    let normalized = host.to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.len() > 253
        || normalized.ends_with('.')
        || normalized.contains("..")
        || normalized.contains(':')
        || !normalized.split('.').all(valid_dns_label)
    {
        return None;
    }
    let parsed = url::Url::parse(&format!("https://{normalized}/")).ok()?;
    if parsed.port().is_some() || parsed.host_str()? != normalized {
        return None;
    }
    Some(normalized)
}

fn valid_dns_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && label
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn https_host_from_url(remote_url: &str) -> Option<String> {
    let authority = remote_url.strip_prefix("https://")?.split('/').next()?;
    if authority.contains(':') {
        return None;
    }
    let parsed = url::Url::parse(remote_url).ok()?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
    {
        return None;
    }
    normalize_git_target("https", parsed.host_str()?)
}

/// Outcome of a git operation ran through the vended-credential fallback.
pub(super) enum CredentialedOutcome<T> {
    /// Succeeded without any credential.
    Plain(T),
    /// Failed plainly, succeeded on the single vended-credential retry.
    Vended(T),
    Failed(anyhow::Error),
}

/// Run a git operation, and on failure retry it once with a vended credential
/// when the repo qualifies. The closure receives the credential to inject (or
/// `None` for the plain attempt) so callers decide how the git command runs.
pub(super) fn with_vended_credential_retry<T>(
    source: Option<&GitCredentialSource>,
    local_id: &str,
    attempt: impl Fn(Option<&BrokeredCredentialHelper>) -> Result<T>,
) -> CredentialedOutcome<T> {
    let original = match attempt(None) {
        Ok(value) => return CredentialedOutcome::Plain(value),
        Err(error) => error,
    };
    let Some(source) = source.filter(|source| source.should_vend(local_id)) else {
        return CredentialedOutcome::Failed(original);
    };
    match source.helper(local_id) {
        Ok(helper) => match attempt(Some(&helper)) {
            Ok(value) => CredentialedOutcome::Vended(value),
            Err(retry_error) => {
                CredentialedOutcome::Failed(anyhow!("{retry_error:#}; {NO_ACCESS_HINT}"))
            }
        },
        Err(helper_error) => CredentialedOutcome::Failed(anyhow!(
            "{original:#}; credential helper setup failed: {helper_error:#}; {NO_ACCESS_HINT}"
        )),
    }
}

/// Run Git with a one-process, exact-host credential helper. The helper itself
/// retrieves the token subject's credential over Git's stdin/stdout protocol;
/// this parent process never receives the username or password.
pub(super) fn git_with_brokered_credential<I, S>(
    cwd: &Path,
    args: I,
    helper: &BrokeredCredentialHelper,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let executable = std::env::current_exe().context("failed to resolve the knit executable")?;
    let executable = executable
        .to_str()
        .context("knit executable path is not valid UTF-8")?;
    let command = format!(
        "!{} git-credential --remote {}",
        shell_quote(executable),
        shell_quote(&helper.remote_name)
    );
    let key = format!("credential.https://{}.helper", helper.host);
    let mut git_args = vec![
        OsString::from("-c"),
        OsString::from(format!("{key}=")),
        OsString::from("-c"),
        OsString::from(format!("{key}={command}")),
    ];
    git_args.extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
    git_output(cwd, git_args)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brokered_git_run_has_no_raw_credential_channel() {
        let dir = std::env::temp_dir();
        let helper = BrokeredCredentialHelper {
            remote_name: "moonbase".to_string(),
            host: "forge.example".to_string(),
        };
        let output = git_with_brokered_credential(&dir, ["version"], &helper).unwrap();
        assert!(output.contains("git version"));
    }

    #[test]
    fn retry_helper_uses_vended_credential_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let source = source_with_repo("backend", Some("private"));
        let attempts = AtomicU32::new(0);
        let public = source_with_repo("backend", Some("public"));
        let outcome = with_vended_credential_retry(Some(&public), "backend", |credential| {
            attempts.fetch_add(1, Ordering::Relaxed);
            match credential {
                None => anyhow::bail!("plain attempt fails"),
                Some(_) => Ok("vended"),
            }
        });
        // Public repo: no vend attempted, single failed attempt.
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert!(matches!(outcome, CredentialedOutcome::Failed(_)));

        let attempts = AtomicU32::new(0);
        let outcome = with_vended_credential_retry(Some(&source), "backend", |helper| {
            attempts.fetch_add(1, Ordering::Relaxed);
            match helper {
                None => anyhow::bail!("plain attempt fails"),
                Some(helper) => Ok(helper.host.clone()),
            }
        });
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        assert!(matches!(outcome, CredentialedOutcome::Vended(host) if host == "forge.example"));

        // Unknown repo id: no vend either.
        let outcome = with_vended_credential_retry(Some(&source), "unknown", |_| {
            anyhow::bail!("plain attempt fails");
            #[allow(unreachable_code)]
            Ok::<&str, anyhow::Error>("unreachable")
        });
        assert!(matches!(outcome, CredentialedOutcome::Failed(_)));

        // No source at all: failure passes straight through.
        let outcome =
            with_vended_credential_retry(None, "backend", |_| Ok::<_, anyhow::Error>("plain"));
        assert!(matches!(outcome, CredentialedOutcome::Plain("plain")));
    }

    fn source_with_repo(local_id: &str, visibility: Option<&str>) -> GitCredentialSource {
        let mut repos = BTreeMap::new();
        repos.insert(
            local_id.to_string(),
            VendRepo {
                host: Some("forge.example".to_string()),
                visibility: visibility.map(ToString::to_string),
            },
        );
        GitCredentialSource {
            remote_name: "moonbase".to_string(),
            repos,
        }
    }

    #[test]
    fn should_vend_requires_non_public_and_https_host() {
        assert!(source_with_repo("backend", Some("private")).should_vend("backend"));
        assert!(source_with_repo("backend", None).should_vend("backend"));
        assert!(!source_with_repo("backend", Some("public")).should_vend("backend"));
        assert!(!source_with_repo("backend", Some("private")).should_vend("other"));
    }

    #[test]
    fn target_normalization_rejects_credential_forwarding_shapes() {
        assert_eq!(
            normalize_git_target("https", "FORGE.EXAMPLE"),
            Some("forge.example".to_string())
        );
        for (protocol, host) in [
            ("http", "forge.example"),
            ("https", "forge.example:443"),
            ("https", "forge.example."),
            ("https", "forge..example"),
            ("https", "forge._example"),
            ("https", "forge.-example"),
            ("https", "fоrge.example"),
            ("https", "user@forge.example"),
        ] {
            assert_eq!(normalize_git_target(protocol, host), None, "{host}");
        }
    }

    #[test]
    fn extracts_only_plain_https_hosts() {
        assert_eq!(
            https_host_from_url("https://forge.example/owner/repo.git"),
            Some("forge.example".to_string())
        );
        assert_eq!(https_host_from_url("http://forge.example/a/b"), None);
        assert_eq!(https_host_from_url("ssh://git@forge.example/a/b"), None);
        assert_eq!(https_host_from_url("https://token@forge.example/a/b"), None);
        assert_eq!(https_host_from_url("https://forge.example:443/a/b"), None);
    }
}
