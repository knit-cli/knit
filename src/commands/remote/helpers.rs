//! Persistent exact-host Git credential helpers backed by `knit git-credential`.
//!
//! Knit owns the helper contract end to end: the helper command syntax, which
//! hosts qualify (the remote's connected forges, exact-HTTPS-host validated),
//! and stale-entry cleanup. Entries live in the user-level (global) Git config
//! — the same private scope the helper itself reads its remote from — so a
//! repository-controlled workspace config can never redirect them.

use super::client::request;
use super::credentials::normalize_git_target;
use crate::model::KnitRemote;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

const HELPER_MARKER: &str = " git-credential --remote ";
const FORGE_CREDENTIAL_SCOPE: &str = "forge:credential";
const ENVIRONMENT_TOKEN_KIND: &str = "environment_client";

/// A global-config helper value written by any knit binary for any remote name.
pub(crate) fn is_knit_helper(value: &str) -> bool {
    value.starts_with('!') && value.contains(HELPER_MARKER)
}

fn is_knit_helper_for(value: &str, remote_name: &str) -> bool {
    is_knit_helper(value)
        && value.ends_with(&format!("{HELPER_MARKER}{}", shell_quote(remote_name)))
}

pub(super) fn helper_command(remote_name: &str) -> Result<String> {
    let executable = std::env::current_exe().context("failed to resolve the knit executable")?;
    let executable = executable
        .to_str()
        .context("knit executable path is not valid UTF-8")?;
    Ok(format!(
        "!{}{HELPER_MARKER}{}",
        shell_quote(executable),
        shell_quote(remote_name)
    ))
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Install our helper on every connected forge host of `remote_name` and drop
/// stale knit-shaped entries (old remote names, disconnected forges) anywhere
/// else. Returns the hosts that now carry the helper.
pub(crate) fn sync_remote_helpers(
    remote_name: &str,
    remote: &KnitRemote,
    token: &str,
) -> Result<BTreeSet<String>> {
    let hosts = connected_forge_hosts(remote, token)?;
    let helper = helper_command(remote_name)?;
    let desired: BTreeMap<String, String> = hosts
        .iter()
        .map(|host| (host.clone(), helper.clone()))
        .collect();
    converge(&desired, RemoveScope::AnyRemote)?;
    Ok(hosts)
}

/// Best-effort helper install before clone/fetch traffic. Only acts when the
/// remote is configured in the user-level config — the single scope
/// `knit git-credential` reads — so a workspace-configured remote never gets
/// a helper that would fail at credential time. Failures degrade to plain
/// Git: public repos still work, private ones fail with the access hint.
pub(crate) fn ensure_helpers_for_git(remote_name: &str) {
    let Ok(config) = crate::store::load_global_config() else {
        return;
    };
    let remote_name = crate::ids::slugify(remote_name);
    let Some(remote) = config.remotes.get(&remote_name) else {
        return;
    };
    let Ok(token) = super::client::resolve_token(&remote_name, remote) else {
        return;
    };
    // Ordinary ledger tokens intentionally cannot export forge secrets.
    // Check that capability before an optional helper request, so working
    // local Git credentials do not produce a misleading permission warning.
    if local_git_only(&introspect_token(remote, &token)) {
        return;
    }
    match sync_remote_helpers(&remote_name, remote, &token) {
        Ok(hosts) if !hosts.is_empty() => {
            crate::human!(
                "{} {} {}",
                crate::output::heading("Credential helper:"),
                hosts.iter().cloned().collect::<Vec<_>>().join(", "),
                crate::output::muted(format!("(remote {remote_name})"))
            );
        }
        Ok(_) => {}
        Err(error) => {
            crate::human!(
                "{}",
                crate::output::muted(format!("credential helper setup skipped: {error:#}"))
            );
        }
    }
}

/// Remove our helper entries. `Some(name)` removes only entries written for
/// that remote name; `None` removes every knit-shaped entry.
pub(crate) fn remove_remote_helpers(remote_name: Option<&str>) -> Result<()> {
    let scope = match remote_name {
        Some(name) => RemoveScope::Remote(name.to_string()),
        None => RemoveScope::AnyRemote,
    };
    converge(&BTreeMap::new(), scope)
}

enum RemoveScope {
    /// Undesired knit-shaped entries are removed whatever remote they name.
    AnyRemote,
    /// Only undesired entries naming this remote are removed.
    Remote(String),
}

/// Converge the global config on `desired_by_host`: our helper leads the list
/// where a host is desired, in-scope knit entries disappear elsewhere, and
/// foreign helpers are preserved in place. Mirrors the shape the environment
/// runtime historically wrote, so existing entries converge instead of piling
/// up.
fn converge(desired_by_host: &BTreeMap<String, String>, scope: RemoveScope) -> Result<()> {
    let entries = read_helper_entries()?;
    let mut keys: BTreeSet<String> = entries.keys().cloned().collect();
    for host in desired_by_host.keys() {
        keys.insert(format!("credential.https://{host}.helper"));
    }
    for key in keys {
        let Some(host) = key
            .strip_prefix("credential.https://")
            .and_then(|rest| rest.strip_suffix(".helper"))
        else {
            continue;
        };
        let existing = entries.get(&key).cloned().unwrap_or_default();
        let retained: Vec<String> = existing
            .iter()
            .filter(|value| match &scope {
                RemoveScope::AnyRemote => !is_knit_helper(value),
                RemoveScope::Remote(name) => {
                    !is_knit_helper(value) || !is_knit_helper_for(value, name)
                }
            })
            .cloned()
            .collect();
        let desired: Vec<String> = match desired_by_host.get(host) {
            Some(helper) => std::iter::once(helper.clone())
                .chain(retained.iter().filter(|value| *value != helper).cloned())
                .collect(),
            None => retained,
        };
        if desired == existing {
            continue;
        }
        if !existing.is_empty() {
            git_config(&["--unset-all", &key])?;
        }
        for value in &desired {
            git_config(&["--add", &key, value])?;
        }
    }
    Ok(())
}

fn read_helper_entries() -> Result<BTreeMap<String, Vec<String>>> {
    let output = Command::new("git")
        .args([
            "config",
            "--global",
            "--null",
            "--get-regexp",
            r"^credential\.https://.+\.helper$",
        ])
        .output()
        .context("failed to run git config")?;
    // Exit code 1 with empty output means no matching entries.
    if !output.status.success() {
        if output.stdout.is_empty() {
            return Ok(BTreeMap::new());
        }
        bail!(
            "git config --get-regexp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("git config output is not UTF-8")?;
    let mut entries: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in stdout.split('\0') {
        if record.is_empty() {
            continue;
        }
        let (key, value) = match record.split_once('\n') {
            Some((key, value)) => (key, value),
            None => (record, ""),
        };
        entries
            .entry(key.to_string())
            .or_default()
            .push(value.to_string());
    }
    Ok(entries)
}

fn git_config(args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("config")
        .arg("--global")
        .args(args)
        .output()
        .context("failed to run git config")?;
    if !output.status.success() {
        bail!(
            "git config {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Optional host discovery obeys the same capability gate as helper setup.
pub(super) fn automatic_forge_hosts(remote: &KnitRemote, token: &str) -> Result<BTreeSet<String>> {
    if local_git_only(&introspect_token(remote, token)) {
        return Ok(BTreeSet::new());
    }
    connected_forge_hosts(remote, token)
}

/// Connected forge hosts, exact-HTTPS-host validated regardless of the server response.
pub(super) fn connected_forge_hosts(remote: &KnitRemote, token: &str) -> Result<BTreeSet<String>> {
    #[derive(Deserialize)]
    struct Descriptor {
        connected: bool,
        hosts: Vec<String>,
    }

    #[derive(Deserialize)]
    struct Envelope {
        data: Vec<Descriptor>,
    }

    let response = request(remote, token, "GET", "/me/forge-credentials", None)?;
    if response.status == 403 {
        // The server refuses to export forge credentials to this token.
        // Classify the denial from the token's own non-secret metadata so
        // the ordinary case points at clone-time collection while an
        // environment-helper denial stays a distinct, actionable error.
        bail!(
            "{}",
            forge_forbidden_diagnosis(introspect_token(remote, token), response.body.trim())
        );
    }
    if !(200..300).contains(&response.status) {
        bail!(
            "Sync remote returned HTTP {}: {}",
            response.status,
            response.body.trim()
        );
    }
    let envelope: Envelope =
        serde_json::from_str(&response.body).context("failed to parse forge credential list")?;
    Ok(envelope
        .data
        .into_iter()
        .filter(|descriptor| descriptor.connected)
        .flat_map(|descriptor| descriptor.hosts)
        .filter_map(|host| normalize_git_target("https", &host))
        .collect())
}

/// What the sync remote said about the bearer token, asked through
/// `/me/access-token`, which returns classification metadata only — never
/// the secret itself — to any valid token.
enum Introspection {
    /// 2xx with a parseable body. `scopes` is `None` when the response
    /// carried no scope metadata at all.
    Token {
        scopes: Option<Vec<String>>,
        kind: Option<String>,
    },
    /// The server rejected the token itself as a bearer (HTTP 401):
    /// invalid, expired, or revoked.
    Rejected,
    /// Anything else — an older remote without the route, an unreadable
    /// body, or a transport failure. `reason` carries no token material.
    Unclassified(String),
}

fn local_git_only(introspection: &Introspection) -> bool {
    matches!(introspection,
        Introspection::Token { scopes: Some(scopes), kind }
            if kind.as_deref() != Some(ENVIRONMENT_TOKEN_KIND)
                && !scopes.iter().any(|scope| scope == FORGE_CREDENTIAL_SCOPE)
    )
}

fn introspect_token(remote: &KnitRemote, token: &str) -> Introspection {
    match request(remote, token, "GET", "/me/access-token", None) {
        Ok(response) => introspection_from_response(response.status, &response.body),
        Err(error) => {
            Introspection::Unclassified(format!("the introspection request failed: {error:#}"))
        }
    }
}

fn introspection_from_response(status: u16, body: &str) -> Introspection {
    if status == 401 {
        // An authentication denial: the bearer itself is not accepted.
        // A 403 here would only prove the endpoint is off-limits, not that
        // the token is bad, so it stays unclassified below.
        return Introspection::Rejected;
    }
    if !(200..300).contains(&status) {
        return Introspection::Unclassified(format!("token introspection returned HTTP {status}"));
    }
    #[derive(Deserialize)]
    struct Descriptor {
        scopes: Option<Vec<String>>,
        #[serde(rename = "tokenKind")]
        token_kind: Option<String>,
    }
    #[derive(Deserialize)]
    struct Envelope {
        data: Descriptor,
    }
    match serde_json::from_str::<Envelope>(body) {
        Ok(envelope) => Introspection::Token {
            scopes: envelope.data.scopes,
            kind: envelope.data.token_kind,
        },
        Err(_) => Introspection::Unclassified(
            "the token introspection response was not valid JSON".to_string(),
        ),
    }
}

/// Explain a 403 from `/me/forge-credentials` using what introspection
/// established. Only one branch names the token type as the cause — the
/// one where the scope metadata proves it.
fn forge_forbidden_diagnosis(introspection: Introspection, server_body: &str) -> String {
    match introspection {
        Introspection::Token { scopes, kind } => {
            let environment_kind = kind.as_deref() == Some(ENVIRONMENT_TOKEN_KIND);
            let scoped = scopes
                .as_ref()
                .is_some_and(|scopes| scopes.iter().any(|scope| scope == FORGE_CREDENTIAL_SCOPE));
            if environment_kind || scoped {
                // Either signal alone does not make the token eligible; say
                // what introspection showed and what to do about it.
                let evidence = if environment_kind {
                    format!(
                        "tokenKind `{ENVIRONMENT_TOKEN_KIND}` {} the `{FORGE_CREDENTIAL_SCOPE}` scope",
                        if scoped { "with" } else { "without" }
                    )
                } else {
                    format!(
                        "the `{FORGE_CREDENTIAL_SCOPE}` scope on a token that is not an \
                         environment client token (tokenKind `{}`)",
                        kind.as_deref().unwrap_or("unknown")
                    )
                };
                format!(
                    "Sync remote returned HTTP 403: the server denied this token's environment \
                     helper authorization — {evidence}. Recheck with `knit remote auth-status \
                     <name>`; if the environment changed or a newer mint superseded this token, \
                     re-mint the environment's client token and retry."
                )
            } else if let Some(scopes) = scopes {
                format!(
                    "Sync remote returned HTTP 403: this sync token cannot export forge \
                     credentials — its scopes ({}) lack the reserved `{FORGE_CREDENTIAL_SCOPE}` \
                     scope, granted only to tokens minted for a registered personal \
                     environment. Use your existing Git credentials or run `knit auth` to \
                     configure local forge defaults; the sync token remains separate.",
                    scopes.join(", ")
                )
            } else {
                format!(
                    "Sync remote returned HTTP 403: {} (denial not classified: introspection \
                     showed no scope metadata, tokenKind `{}`). Recheck with `knit remote \
                     auth-status <name>` before changing anything.",
                    server_body.trim(),
                    kind.as_deref().unwrap_or("unknown")
                )
            }
        }
        Introspection::Rejected => {
            "Sync remote returned HTTP 403, and /me/access-token rejected the sync token \
             (HTTP 401): the token is invalid, expired, or revoked. Set a fresh one with `knit \
             remote add <name> <url> --global --token-stdin` or the `KNIT_REMOTE_<NAME>_TOKEN` \
             environment variable."
                .to_string()
        }
        Introspection::Unclassified(reason) => format!(
            "Sync remote returned HTTP 403: {} (denial not classified: {reason}). Recheck with \
             `knit remote auth-status <name>` before changing anything.",
            server_body.trim()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_helpers_skip_only_known_ordinary_sync_tokens() {
        for kind in [None, Some("legacy"), Some("personal")] {
            assert!(local_git_only(&Introspection::Token {
                scopes: Some(vec!["project:read".into(), "bundle:read".into()]),
                kind: kind.map(str::to_owned),
            }));
        }
        for (kind, scopes) in [
            (Some(ENVIRONMENT_TOKEN_KIND), Some(vec![])),
            (
                Some(ENVIRONMENT_TOKEN_KIND),
                Some(vec![FORGE_CREDENTIAL_SCOPE.into()]),
            ),
            (Some("legacy"), Some(vec![FORGE_CREDENTIAL_SCOPE.into()])),
            (Some("legacy"), None),
        ] {
            assert!(!local_git_only(&Introspection::Token {
                scopes,
                kind: kind.map(str::to_owned),
            }));
        }
        assert!(!local_git_only(&Introspection::Rejected));
        assert!(!local_git_only(&Introspection::Unclassified(
            "unavailable".into()
        )));
    }

    #[test]
    fn knit_helper_shape_is_recognized_across_names_and_paths() {
        assert!(is_knit_helper(
            "!'/usr/local/bin/knit' git-credential --remote 'hosted'"
        ));
        assert!(is_knit_helper(
            "!'/other/knit' git-credential --remote 'moonbase'"
        ));
        assert!(!is_knit_helper("osxkeychain"));
        assert!(!is_knit_helper("!git-credential-manager"));
        assert!(is_knit_helper_for(
            "!'/usr/local/bin/knit' git-credential --remote 'hosted'",
            "hosted"
        ));
        assert!(!is_knit_helper_for(
            "!'/usr/local/bin/knit' git-credential --remote 'hosted'",
            "moonbase"
        ));
    }

    #[test]
    fn helper_command_quotes_the_remote_name() {
        let command = helper_command("moon base").unwrap();
        assert!(command.starts_with('!'));
        assert!(command.ends_with(" git-credential --remote 'moon base'"));
    }

    const FORBIDDEN_BODY: &str = r#"{"errors":{"detail":"Forbidden"}}"#;

    fn diagnosis_for(status: u16, body: &str) -> String {
        forge_forbidden_diagnosis(introspection_from_response(status, body), FORBIDDEN_BODY)
    }

    #[test]
    fn ordinary_scope_metadata_points_at_local_auth() {
        let diagnosis = diagnosis_for(
            200,
            r#"{"data":{"tokenKind":"legacy","scopes":["project:read","bundle:read","artifact:read","history:read"]}}"#,
        );
        assert!(diagnosis.contains("project:read, bundle:read, artifact:read, history:read"));
        assert!(diagnosis.contains("registered personal environment"));
        assert!(diagnosis.contains("knit auth"));
        assert!(diagnosis.contains("sync token remains separate"));
        assert!(!diagnosis.contains("auth add"));
        assert!(!diagnosis.contains("--credential"));
    }

    #[test]
    fn environment_helper_denial_stays_distinct_from_ordinary_advice() {
        let diagnosis = diagnosis_for(
            200,
            r#"{"data":{"tokenKind":"environment_client","environmentId":"env-1","scopes":["bundle:read","forge:credential"]}}"#,
        );
        assert!(diagnosis.contains("denied this token's environment helper authorization"));
        assert!(
            diagnosis.contains("tokenKind `environment_client` with the `forge:credential` scope")
        );
        assert!(diagnosis.contains("re-mint"));
        assert!(!diagnosis.contains("should be able"));
        assert!(!diagnosis.contains("auth add"));

        // The token kind alone is not proof of eligibility: report the
        // missing scope as evidence instead of claiming the token qualifies.
        let kind_only = diagnosis_for(
            200,
            r#"{"data":{"tokenKind":"environment_client","scopes":["bundle:read"]}}"#,
        );
        assert!(kind_only.contains("without the `forge:credential` scope"));
        assert!(kind_only.contains("denied this token's environment helper authorization"));
        assert!(!kind_only.contains("should be able"));

        // The scope without the environment kind is a binding mismatch, not
        // an environment token.
        let scope_only = diagnosis_for(
            200,
            r#"{"data":{"tokenKind":"legacy","scopes":["forge:credential"]}}"#,
        );
        assert!(scope_only.contains("not an environment client token"));
        assert!(scope_only.contains("re-mint"));
        assert!(!scope_only.contains("should be able"));
    }

    #[test]
    fn missing_scope_metadata_is_not_labelled_ordinary() {
        let diagnosis = diagnosis_for(200, r#"{"data":{"tokenKind":"legacy","subject":"user-1"}}"#);
        assert!(diagnosis.contains("denial not classified"));
        assert!(diagnosis.contains("no scope metadata"));
        assert!(diagnosis.contains(FORBIDDEN_BODY));
        assert!(!diagnosis.contains("auth add"));
        assert!(!diagnosis.contains("lack the reserved"));
    }

    #[test]
    fn malformed_introspection_body_falls_back_to_unclassified() {
        let diagnosis = diagnosis_for(200, "not json");
        assert!(diagnosis.contains("denial not classified"));
        assert!(diagnosis.contains("not valid JSON"));
        assert!(diagnosis.contains(FORBIDDEN_BODY));
    }

    #[test]
    fn unsupported_introspection_status_is_reported_without_guessing() {
        let diagnosis = diagnosis_for(404, r#"{"data":{}}"#);
        assert!(diagnosis.contains("denial not classified"));
        assert!(diagnosis.contains("token introspection returned HTTP 404"));
        assert!(!diagnosis.contains("auth add"));
    }

    #[test]
    fn refused_introspection_does_not_claim_the_token_is_revoked() {
        // A 403 from introspection only proves the endpoint is off-limits.
        let diagnosis = diagnosis_for(403, r#"{"errors":{"detail":"Forbidden"}}"#);
        assert!(diagnosis.contains("denial not classified"));
        assert!(diagnosis.contains("token introspection returned HTTP 403"));
        assert!(!diagnosis.contains("revoked"));
        assert!(!diagnosis.contains("invalid, expired"));
    }

    #[test]
    fn rejected_sync_token_points_at_stdin_token_rotation() {
        let diagnosis = diagnosis_for(401, r#"{"errors":{"detail":"Unauthorized"}}"#);
        assert!(diagnosis.contains("rejected the sync token"));
        assert!(diagnosis.contains("HTTP 401"));
        assert!(diagnosis.contains("knit remote add <name> <url> --global --token-stdin"));
        assert!(diagnosis.contains("KNIT_REMOTE_<NAME>_TOKEN"));
        assert!(!diagnosis.contains("remote token <name> <token>"));
    }
}
