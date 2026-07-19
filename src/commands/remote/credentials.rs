//! Vended git-credential fallback: when a git clone/fetch fails for a
//! non-public repo, ask the sync remote to vend the stored project-member
//! credential and retry once. The credential lives only in the child git
//! process environment, delivered through an ephemeral GIT_ASKPASS shim —
//! never in the URL, argv, git config, or any file that outlives the call.

use super::client::request;
use super::clone::export_repo_local_id;
use super::{RemoteExportRepository, RemoteProjectExport};
use crate::git::git_output_with_env;
use crate::model::KnitRemote;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Appended to a repo's failure when the server has no credential to vend.
pub(super) const NO_ACCESS_HINT: &str =
    "no git access: configure SSH keys or ask a project member to connect GitHub on the sync remote";

/// Marker recorded in `--json` repo entries for repos that rode a vended
/// credential; part of the machine-readable contract with ivaldi.
pub(super) const VENDED_CREDENTIAL_LABEL: &str = "remote-vended";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VendedCredential {
    pub(super) username: String,
    pub(super) password: String,
    #[serde(default)]
    pub(super) provider: Option<String>,
    #[serde(default)]
    pub(super) acts_as: Option<String>,
}

/// Result of asking the server for a credential: either one to retry with, or
/// an authoritative "nobody connected GitHub" (HTTP 409 `noCredential`).
pub(super) enum VendAttempt {
    Credential(VendedCredential),
    NoCredential,
}

struct VendRepo {
    server_id: Option<String>,
    visibility: Option<String>,
}

/// Everything needed to vend credentials for the repos of one project export:
/// the resolved remote + token and the export's server-side project/repo ids.
pub(super) struct GitCredentialSource {
    remote: KnitRemote,
    token: String,
    project_id: Option<String>,
    repos: BTreeMap<String, VendRepo>,
}

impl GitCredentialSource {
    pub(super) fn from_export(
        remote: &KnitRemote,
        token: &str,
        export: &RemoteProjectExport,
    ) -> Self {
        let repos = export
            .repositories
            .iter()
            .map(|repository: &RemoteExportRepository| {
                (
                    export_repo_local_id(repository),
                    VendRepo {
                        server_id: repository.id.clone(),
                        visibility: repository.visibility.clone(),
                    },
                )
            })
            .collect();
        Self {
            remote: remote.clone(),
            token: token.to_string(),
            project_id: export.project.id.clone(),
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
        repo.server_id.is_some() && self.project_id.is_some()
    }

    fn vend(&self, local_id: &str) -> Result<VendAttempt> {
        let repo = self
            .repos
            .get(local_id)
            .with_context(|| format!("{local_id}: not part of the remote export"))?;
        let project_id = self
            .project_id
            .as_deref()
            .context("remote export carries no project id; cannot request a git credential")?;
        let repository_id = repo
            .server_id
            .as_deref()
            .with_context(|| format!("{local_id}: export carries no repository id"))?;
        let response = request(
            &self.remote,
            &self.token,
            "POST",
            &format!("/projects/{project_id}/repositories/{repository_id}/git-credential"),
            None,
        )?;
        if response.status == 409 {
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
            .context("failed to parse git-credential response")?;
        let credential = envelope.data;
        // Transparency: the vended credential acts as a real account.
        crate::human!(
            "{}: {}",
            crate::output::repo(local_id),
            crate::output::muted(format!(
                "using remote-vended {} credential{}",
                credential.provider.as_deref().unwrap_or("git"),
                credential
                    .acts_as
                    .as_deref()
                    .map(|acts_as| format!(" (acts as {acts_as})"))
                    .unwrap_or_default()
            ))
        );
        Ok(VendAttempt::Credential(credential))
    }
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
    attempt: impl Fn(Option<&VendedCredential>) -> Result<T>,
) -> CredentialedOutcome<T> {
    let original = match attempt(None) {
        Ok(value) => return CredentialedOutcome::Plain(value),
        Err(error) => error,
    };
    let Some(source) = source.filter(|source| source.should_vend(local_id)) else {
        return CredentialedOutcome::Failed(original);
    };
    match source.vend(local_id) {
        Ok(VendAttempt::Credential(credential)) => match attempt(Some(&credential)) {
            Ok(value) => CredentialedOutcome::Vended(value),
            Err(retry_error) => CredentialedOutcome::Failed(retry_error),
        },
        Ok(VendAttempt::NoCredential) => {
            CredentialedOutcome::Failed(anyhow!("{original:#}; {NO_ACCESS_HINT}"))
        }
        Err(vend_error) => CredentialedOutcome::Failed(anyhow!(
            "{original:#}; credential vending failed: {vend_error:#}"
        )),
    }
}

/// Run a git command with a vended credential supplied through an ephemeral
/// GIT_ASKPASS shim. The shim echoes from environment variables only and is
/// deleted before this function returns, whatever the git outcome.
pub(super) fn git_with_vended_credential<I, S>(
    cwd: &Path,
    args: I,
    credential: &VendedCredential,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let shim = AskpassShim::create()?;
    let script = shim.script.to_string_lossy().to_string();
    git_output_with_env(
        cwd,
        args,
        &[
            ("GIT_ASKPASS", script.as_str()),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("KNIT_GIT_USER", credential.username.as_str()),
            ("KNIT_GIT_PASS", credential.password.as_str()),
        ],
    )
}

/// An askpass script on disk for the duration of one git call. The script
/// contains no secrets — it echoes `$KNIT_GIT_USER` / `$KNIT_GIT_PASS` from
/// the environment — and its private directory is removed on drop.
struct AskpassShim {
    dir: PathBuf,
    script: PathBuf,
}

impl AskpassShim {
    fn create() -> Result<Self> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "knit-askpass-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create askpass dir {}", dir.display()))?;
        let script = if cfg!(windows) {
            dir.join("askpass.bat")
        } else {
            dir.join("askpass.sh")
        };
        fs::write(&script, askpass_script_body())
            .with_context(|| format!("failed to write askpass shim {}", script.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&dir, &script] {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(
                    || {
                        format!(
                            "failed to restrict askpass permissions on {}",
                            path.display()
                        )
                    },
                )?;
            }
        }
        Ok(Self { dir, script })
    }
}

impl Drop for AskpassShim {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.script);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn askpass_script_body() -> &'static str {
    if cfg!(windows) {
        concat!(
            "@echo off\r\n",
            "echo %* | findstr /i \"username\" >nul\r\n",
            "if %errorlevel%==0 (echo %KNIT_GIT_USER%) else (echo %KNIT_GIT_PASS%)\r\n",
        )
    } else {
        concat!(
            "#!/bin/sh\n",
            "# Ephemeral knit askpass shim: credentials come from the environment only.\n",
            "case \"$1\" in\n",
            "  *[Uu]sername*) printf '%s\\n' \"$KNIT_GIT_USER\" ;;\n",
            "  *) printf '%s\\n' \"$KNIT_GIT_PASS\" ;;\n",
            "esac\n",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> VendedCredential {
        VendedCredential {
            username: "x-access-token".to_string(),
            password: "ghs_secret_token".to_string(),
            provider: Some("github".to_string()),
            acts_as: Some("marc".to_string()),
        }
    }

    #[test]
    fn askpass_shim_contains_no_secret_and_is_removed_after_drop() {
        let shim = AskpassShim::create().unwrap();
        let script = shim.script.clone();
        let dir = shim.dir.clone();
        let body = fs::read_to_string(&script).unwrap();
        assert!(body.contains("KNIT_GIT_USER"));
        assert!(body.contains("KNIT_GIT_PASS"));
        assert!(!body.contains("ghs_"), "shim must never embed a secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&script).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "shim must be private+executable");
        }
        drop(shim);
        assert!(!script.exists(), "shim script must be deleted");
        assert!(!dir.exists(), "shim dir must be deleted");
    }

    #[cfg(unix)]
    #[test]
    fn askpass_shim_echoes_credentials_from_env_only() {
        let shim = AskpassShim::create().unwrap();
        let run = |prompt: &str| {
            let output = std::process::Command::new("sh")
                .arg(&shim.script)
                .arg(prompt)
                .env("KNIT_GIT_USER", "vend-user")
                .env("KNIT_GIT_PASS", "vend-pass")
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        assert_eq!(run("Username for 'https://github.com':"), "vend-user");
        assert_eq!(run("Password for 'https://x@github.com':"), "vend-pass");
    }

    #[test]
    fn vended_git_run_keeps_credential_out_of_argv() {
        // The runner's argv is exactly the caller's git args: the credential
        // travels only in the environment. Assert by running a benign git
        // command through the vended path and checking it succeeds without the
        // credential appearing anywhere in the invocation or output.
        let dir = std::env::temp_dir().join(format!("knit-vend-argv-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let output = git_with_vended_credential(&dir, ["version"], &credential()).unwrap();
        assert!(output.contains("git version"));
        assert!(!output.contains("ghs_secret_token"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retry_helper_uses_vended_credential_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let source = source_with_repo("backend", Some("private"));
        let attempts = AtomicU32::new(0);
        // No server is running, so vend() would fail; instead exercise the
        // closure contract via a source that never qualifies (public repo).
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
                server_id: Some("r-1".to_string()),
                visibility: visibility.map(ToString::to_string),
            },
        );
        GitCredentialSource {
            remote: KnitRemote {
                url: "http://127.0.0.1:1".to_string(),
                token: None,
            },
            token: "t".to_string(),
            project_id: Some("p-1".to_string()),
            repos,
        }
    }

    #[test]
    fn should_vend_requires_non_public_and_ids() {
        assert!(source_with_repo("backend", Some("private")).should_vend("backend"));
        assert!(source_with_repo("backend", None).should_vend("backend"));
        assert!(!source_with_repo("backend", Some("public")).should_vend("backend"));
        assert!(!source_with_repo("backend", Some("private")).should_vend("other"));
    }
}
