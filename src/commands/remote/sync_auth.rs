//! `knit auth remote`: set up or update the token for one named sync remote
//! (the hosted Knit service) in the user-level config, verifying the fresh
//! candidate against the server before anything is saved. Also owns
//! `knit remote auth-status`, which reports the validated sync login without
//! letting the optional forge capability probe fail the command.

use super::client::{
    api_base_url, normalize_base_url, remote_token_env_name, resolve_remote, resolve_token,
};
use crate::ids::slugify;
use crate::model::{KnitConfig, KnitRemote};
use crate::output as out;
use crate::store::{load_global_config, save_global_config};
use crate::token_entry::{read_token, redact};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{self, IsTerminal, Read, Write};
use std::time::Duration;

/// Verification and introspection requests are bounded and never follow
/// redirects, so a hanging or redirecting endpoint cannot stall token entry.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);
/// The optional forge capability probe is even more tightly bounded: it is
/// advisory, never worth waiting on.
const FORGE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Response bodies are read with a hard cap so a hostile or broken endpoint
/// cannot stream unbounded data at a token prompt.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// `knit auth remote [NAME] [--url URL] [--token-stdin] [--offline]`.
///
/// The remote always lives in the user-level config — a repository-controlled
/// workspace config is never consulted or written. A fresh candidate token is
/// always collected (a stored token is never re-sent, least of all to a
/// changed URL) and, unless `--offline`, verified via `GET
/// /api/v1/me/access-token` before the save. Name and URL are validated
/// before any secret is read; an empty submission or cancellation leaves the
/// previous configuration untouched.
pub fn auth_remote(
    name: Option<&str>,
    url: Option<&str>,
    token_stdin: bool,
    offline: bool,
) -> Result<()> {
    let interactive = io::stdin().is_terminal();
    let config = load_global_config()?;
    // Resolve and validate the remote name before reading any secret.
    let remote_name = match name {
        Some(name) => {
            require_nameable(name)?;
            slugify(name)
        }
        None if interactive => prompt_remote_name(&config)?,
        None => bail!(
            "A remote name is required when not interactive. Configured user-level remotes: {}. \
             Use `knit auth remote <name> --token-stdin` (existing remote) or add `--url <url>` \
             for a new one.",
            describe_configured_remotes(&config)
        ),
    };
    let existing = config.remotes.get(&remote_name).cloned();
    // Resolve and validate the endpoint before reading any secret. The stored
    // URL is validated too — a legacy entry with embedded credentials or a
    // query string is refused rather than trusted. Workspace-configured
    // remotes are never considered.
    let base_url = match url {
        Some(url) => validate_service_url(url)?,
        None => match &existing {
            Some(remote) => validate_service_url(&remote.url).with_context(|| {
                format!(
                    "Remote `{remote_name}` has a stored URL that cannot be verified against; \
                     pass `--url <url>` to replace it"
                )
            })?,
            None if interactive => prompt_service_url(&remote_name)?,
            None => bail!(
                "Remote `{remote_name}` is not configured in the user-level config, so there is \
                 no saved URL to reuse. Pass one explicitly: `knit auth remote {remote_name} \
                 --url https://host.example --token-stdin`.",
            ),
        },
    };
    if let Some(old) = &existing {
        if old.url != base_url {
            println!(
                "{} endpoint changes from {} to {}; the previously stored token is not sent to \
                  the new URL.",
                out::warn("note:"),
                display_url(&old.url),
                base_url
            );
        }
    }
    // Collect and verify a fresh candidate. The stored token and any
    // environment override are deliberately never verified or reused here.
    let prompt = format!(
        "Token for remote `{remote_name}` at {base_url} (input hidden, press Enter to submit): "
    );
    let mut verified = false;
    let token = loop {
        let candidate = collect_candidate(&remote_name, token_stdin, interactive, &prompt)?;
        if offline {
            break candidate;
        }
        println!("Verifying token with {base_url}…");
        match verify_remote_token(&base_url, &candidate) {
            TokenVerification::Verified => {
                verified = true;
                break candidate;
            }
            TokenVerification::Rejected(status) => {
                if interactive {
                    println!(
                        "{} the server at {base_url} rejected this token (HTTP {status}); \
                          nothing was saved. Paste the token again, or press Ctrl-C to cancel \
                          without changing anything.",
                        out::warn("Rejected:")
                    );
                    continue;
                }
                bail!(
                    "The server at {base_url} rejected the token for `{remote_name}` (HTTP \
                     {status}); nothing was saved. Check the token and retry: `printf '%s\\n' \
                     \"$TOKEN\" | knit auth remote {remote_name} --token-stdin`.",
                );
            }
            TokenVerification::Indeterminate(reason) => {
                let reason = redact(&reason, &candidate);
                bail!(
                    "Could not verify the token against {base_url}: {reason}. Nothing was \
                     saved, and this does not prove the token invalid. Check the service and \
                     retry, or pass --offline to save it without verification.",
                );
            }
        }
    };
    // Reload the global config so unrelated remotes edited while the token
    // was being entered survive, and refuse a same-remote concurrent change.
    let mut fresh = load_global_config()?;
    let snapshot = existing
        .as_ref()
        .map(|remote| (remote.url.as_str(), remote.token.as_deref()));
    apply_remote_update(&mut fresh, &remote_name, snapshot, &base_url, &token)?;
    save_global_config(&fresh)?;
    let config_path = crate::store::global_config_path()?;
    println!(
        "{} {} {}",
        out::movement(if existing.is_some() {
            "updated"
        } else {
            "configured"
        }),
        out::repo(&remote_name),
        out::muted(format!("({base_url})"))
    );
    if verified {
        println!(
            "{} token verified with {base_url}.",
            out::heading("Verification:")
        );
    } else {
        println!(
            "{} token saved WITHOUT verification (--offline); check it later with `knit \
             remote auth-status {remote_name}`.",
            out::heading("Verification:")
        );
    }
    println!(
        "{} {}",
        out::heading("Token saved to:"),
        config_path.display()
    );
    println!(
        "{} knit remote auth-status {remote_name}",
        out::heading("Verify:")
    );
    println!(
        "{} knit auth remote {remote_name}",
        out::heading("Replace:")
    );
    if let Some(active) = active_token_env_name(&remote_name) {
        println!(
            "{}",
            out::muted(format!(
                "{active} overrides the stored token from the environment; environment tokens \
                 are never verified by this flow."
            ))
        );
    }
    Ok(())
}

/// A remote name must carry at least one ASCII letter or digit — `slugify`
/// falls back to a generic slug for punctuation-only input, which would
/// silently configure the wrong remote.
fn require_nameable(name: &str) -> Result<()> {
    if !name.bytes().any(|b| b.is_ascii_alphanumeric()) {
        bail!("Remote name `{name}` must contain at least one letter or digit.");
    }
    Ok(())
}

/// The candidate-collection gate: without a terminal the token must arrive
/// on stdin, and the failure explains exactly how, instead of hanging.
fn collect_candidate(
    remote_name: &str,
    token_stdin: bool,
    interactive: bool,
    prompt: &str,
) -> Result<String> {
    if token_stdin || interactive {
        read_token(prompt, "remote token")
    } else {
        bail!(
            "No token provided without a terminal. Pipe one: `printf '%s\\n' \"$TOKEN\" | knit \
             auth remote {remote_name} --token-stdin`. At a terminal, `knit auth remote \
             {remote_name}` prompts for it with input hidden."
        );
    }
}

/// Insert or update one remote in `config`, refusing when the recorded entry
/// changed after the snapshot was taken (a concurrent same-remote edit).
fn apply_remote_update(
    config: &mut KnitConfig,
    remote_name: &str,
    snapshot: Option<(&str, Option<&str>)>,
    base_url: &str,
    token: &str,
) -> Result<()> {
    let current = config
        .remotes
        .get(remote_name)
        .map(|remote| (remote.url.as_str(), remote.token.as_deref()));
    if current != snapshot {
        bail!(
            "Remote `{remote_name}` changed while the token was being entered; nothing was \
             saved. Re-run `knit auth remote {remote_name}` if that change was expected."
        );
    }
    config.remotes.insert(
        remote_name.to_owned(),
        KnitRemote {
            url: base_url.to_owned(),
            token: Some(token.to_owned()),
        },
    );
    Ok(())
}

/// Validate an explicit or prompted service endpoint: a real HTTP(S) host,
/// never embedded credentials, a query, or a fragment.
fn validate_service_url(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("Service URL is empty; provide one like `https://host.example`.");
    }
    if raw.chars().any(char::is_whitespace) {
        bail!("Service URL must not contain whitespace.");
    }
    let parsed = url::Url::parse(raw).map_err(|_| {
        anyhow::anyhow!(
            "Service URL must be a valid HTTP(S) URL, for example `https://host.example`."
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!(
            "Service URL must use http or https, not `{}`.",
            parsed.scheme()
        );
    }
    if parsed.host_str().unwrap_or_default().is_empty() {
        bail!("Service URL has no host; provide one like `https://host.example`.");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        // Never include the rejected URL in the message.
        bail!("Service URL must not embed credentials (user:password@host).");
    }
    if parsed.query().is_some() {
        bail!("Service URL must not include a query string.");
    }
    if parsed.fragment().is_some() {
        bail!("Service URL must not include a fragment.");
    }
    Ok(normalize_base_url(raw))
}

/// A URL safe to display: rebuilt from the parsed value, so legacy stored
/// URLs with embedded userinfo or query strings never leak into listings,
/// notes, or errors. Invalid entries degrade to a placeholder.
fn display_url(raw: &str) -> String {
    let Ok(parsed) = url::Url::parse(raw.trim()) else {
        return "<invalid stored URL>".to_string();
    };
    let host = parsed.host_str().unwrap_or("<no host>");
    let port = parsed
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let path = parsed.path().trim_end_matches('/');
    format!("{}://{host}{port}{path}", parsed.scheme())
}

/// What the verification probe established about the candidate token.
#[derive(Debug)]
enum TokenVerification {
    /// 2xx with a properly shaped envelope: `{"data": {…}}`.
    Verified,
    /// The server rejected the candidate bearer itself (HTTP 401/403).
    Rejected(u16),
    /// Transport failure, redirect, other status, or malformed body. Never
    /// proof the token is invalid. `reason` carries no token material.
    Indeterminate(String),
}

/// A bounded GET response. `body` is read only for 2xx responses; for every
/// other status it stays empty — error payloads can reflect request data and
/// are never surfaced.
struct BoundedResponse {
    status: u16,
    body: String,
}

/// One bounded, redirect-free authenticated GET. Following no redirects and
/// capping both time and body size means a broken or hostile endpoint cannot
/// stall token entry or stream unbounded data. Transport failures return a
/// sanitized message that carries no token material.
fn bounded_bearer_get(
    full_url: &str,
    token: &str,
    timeout: Duration,
) -> std::result::Result<BoundedResponse, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout(timeout)
        .redirects(0)
        .build();
    match agent
        .get(full_url)
        .set("authorization", &format!("Bearer {token}"))
        .call()
    {
        Ok(response) => {
            let status = response.status();
            if !(200..300).contains(&status) {
                // Defensive: ureq yields non-2xx through Err below, but the
                // classification must never depend on that.
                return Ok(BoundedResponse {
                    status,
                    body: String::new(),
                });
            }
            let mut body = String::new();
            let read = std::io::Read::take(response.into_reader(), MAX_RESPONSE_BYTES + 1)
                .read_to_string(&mut body);
            match read {
                Ok(count) if count as u64 > MAX_RESPONSE_BYTES => {
                    Err("the response body exceeded the size limit".to_string())
                }
                Ok(_) => Ok(BoundedResponse { status, body }),
                Err(error) => Err(redact(
                    &format!("the response body could not be read: {error}"),
                    token,
                )),
            }
        }
        Err(ureq::Error::Status(status, _response)) => Ok(BoundedResponse {
            status,
            body: String::new(),
        }),
        Err(ureq::Error::Transport(transport)) => {
            Err(redact(&format!("the request failed: {transport}"), token))
        }
    }
}

/// Verify a candidate token with a single bounded `GET
/// {base}/api/v1/me/access-token` (a `/api/v1` suffix on `base` is honored).
/// Only a successful, properly shaped envelope with an object `data` counts
/// as verified; redirects (3xx) are never followed and never verified.
fn verify_remote_token(base_url: &str, token: &str) -> TokenVerification {
    let url = format!("{}/me/access-token", api_base_url(base_url));
    match bounded_bearer_get(&url, token, VERIFY_TIMEOUT) {
        Ok(response) => {
            if (200..300).contains(&response.status) {
                if envelope_data_is_object(&response.body) {
                    TokenVerification::Verified
                } else {
                    TokenVerification::Indeterminate(
                        "the response was not the expected JSON envelope with a `data` object"
                            .to_string(),
                    )
                }
            } else if matches!(response.status, 401 | 403) {
                TokenVerification::Rejected(response.status)
            } else {
                TokenVerification::Indeterminate(format!(
                    "the server returned HTTP {}",
                    response.status
                ))
            }
        }
        Err(reason) => TokenVerification::Indeterminate(reason),
    }
}

/// `{"data": {…}}` — an object payload is the only shape accepted as proof
/// the server authenticated the candidate.
fn envelope_data_is_object(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("data").cloned())
        .is_some_and(|data| data.is_object())
}

/// The environment variable currently overriding a named remote's stored
/// token, if one is active: the specific `KNIT_REMOTE_<NAME>_TOKEN` first,
/// then the shared `KNIT_REMOTE_TOKEN` — the same precedence `token_from_env`
/// resolves values with. Names only, never values.
fn active_token_env_name(remote_name: &str) -> Option<String> {
    let specific = remote_token_env_name(remote_name);
    let set = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .is_some()
    };
    if set(&specific) {
        Some(specific)
    } else if set("KNIT_REMOTE_TOKEN") {
        Some("KNIT_REMOTE_TOKEN".to_string())
    } else {
        None
    }
}

/// `knit remote auth-status <name>`: introspect the resolved token through
/// `/me/access-token`. The stored endpoint is validated before any token is
/// sent anywhere; a 401/403 gets actionable rotation guidance (naming the
/// active environment override when one is in effect); other failures never
/// claim the token is invalid. The optional `/me/forge-credentials`
/// capability probe is decoupled from the validated sync login: its failure
/// is reported inline (JSON note or muted line) and never fails the command.
/// Nothing is printed before the JSON document in `--json` mode, response
/// bodies are never echoed, and transport errors are sanitized, so a token
/// or a legacy URL's credentials reflected by the server cannot leak.
pub fn remote_auth_status(name: &str, json_output: bool) -> Result<()> {
    // Environment-bound credentials live only in the private user config;
    // workspace config is repository-controlled and must not redirect token
    // introspection to another server.
    let config = load_global_config()?;
    let remote_name = slugify(name);
    let remote = resolve_remote(&config, &remote_name)?;
    // Validate the stored endpoint before a token is sent anywhere; the
    // refusal never echoes the stored URL, which may carry legacy userinfo
    // or a query string.
    let base_url = validate_service_url(&remote.url).with_context(|| {
        format!(
            "Remote `{remote_name}` has a stored URL that cannot be safely requested. Repair it \
             with `knit auth remote {remote_name} --url https://host.example` (add \
             `--token-stdin` to set its token)."
        )
    })?;
    let token = resolve_token(&remote_name, remote).map_err(|_| {
        anyhow::anyhow!(
            "No remote token configured for `{remote_name}`. Set one with `knit auth remote \
             {remote_name}` (hidden prompt at a terminal, `--token-stdin` for pipes), or set \
             the {} environment variable.",
            remote_token_env_name(&remote_name)
        )
    })?;
    let shown_url = display_url(&base_url);
    let mut status: Value = match bounded_bearer_get(
        &format!("{}/me/access-token", api_base_url(&base_url)),
        &token,
        VERIFY_TIMEOUT,
    ) {
        Ok(response) if (200..300).contains(&response.status) => {
            let envelope: Value = serde_json::from_str(&response.body)
                .context("the token introspection response was not valid JSON")?;
            envelope
                .get("data")
                .cloned()
                .filter(|data| data.is_object())
                .context(
                    "the token introspection response was not the expected envelope with a \
                     `data` object",
                )?
        }
        Ok(response) if matches!(response.status, 401 | 403) => {
            match active_token_env_name(&remote_name) {
                Some(variable) => {
                    // Unsetting the named variable is not always enough: a
                    // specific override can shadow a generic one, so the
                    // fallback names both when the specific one is active.
                    let fallback = if variable == "KNIT_REMOTE_TOKEN" {
                        "unset any remote-token environment overrides".to_string()
                    } else {
                        format!("unset both {variable} and KNIT_REMOTE_TOKEN")
                    };
                    bail!(
                        "The sync token for `{remote_name}` was rejected by {shown_url} \
                         (HTTP {}). The active token comes from the {variable} environment \
                         variable, which overrides the stored one — update or unset \
                         {variable}. To use the saved token, {fallback}; replace the saved \
                         token with `knit auth remote {remote_name}` if needed.",
                        response.status
                    )
                }
                None => bail!(
                    "The sync token for `{remote_name}` was rejected by {shown_url} (HTTP {}). \
                     Rotate it with `knit auth remote {remote_name}`, or set the {} environment \
                     variable and retry.",
                    response.status,
                    remote_token_env_name(&remote_name)
                ),
            }
        }
        Ok(response) => {
            bail!(
                "HTTP {} while checking the sync token for `{remote_name}` ({shown_url}). This \
                 does not prove the token invalid; check the service and retry with `knit \
                 remote auth-status {remote_name}`.",
                response.status
            );
        }
        Err(reason) => {
            bail!(
                "Could not reach {shown_url} to check the sync token: {reason}. This does not \
                 prove the token invalid; check the service and retry."
            );
        }
    };
    // Optional capability probe: any failure is reported, never fatal, and
    // never claims anything about the just-validated sync login.
    let mut forge_probe_error: Option<String> = None;
    let forge_credentials: Value = match bounded_bearer_get(
        &format!("{}/me/forge-credentials", api_base_url(&base_url)),
        &token,
        FORGE_PROBE_TIMEOUT,
    ) {
        Ok(response) if (200..300).contains(&response.status) => {
            match serde_json::from_str::<Value>(&response.body) {
                Ok(value) => match value.get("data").cloned() {
                    Some(data) if data.is_array() => data,
                    _ => {
                        forge_probe_error =
                            Some("the response was not a forge credential list".to_string());
                        Value::Array(Vec::new())
                    }
                },
                Err(_) => {
                    forge_probe_error = Some("the response was not valid JSON".to_string());
                    Value::Array(Vec::new())
                }
            }
        }
        Ok(response) => {
            forge_probe_error = Some(format!("HTTP {}", response.status));
            Value::Array(Vec::new())
        }
        Err(reason) => {
            forge_probe_error = Some(reason);
            Value::Array(Vec::new())
        }
    };
    let connected = forge_credentials
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry.get("connected").and_then(Value::as_bool) == Some(true))
                .count()
        })
        .unwrap_or(0);
    if let Some(status) = status.as_object_mut() {
        status.insert("forgeCredentials".to_string(), forge_credentials);
        if let Some(error) = forge_probe_error.clone() {
            status.insert("forgeProbeError".to_string(), Value::String(error));
        }
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    let kind = status
        .get("tokenKind")
        .and_then(Value::as_str)
        .unwrap_or("legacy");
    let subject = status
        .get("subjectUserId")
        .and_then(Value::as_str)
        .unwrap_or("unbound");
    let environment = status
        .get("environmentId")
        .and_then(Value::as_str)
        .unwrap_or("unbound");
    let expiry = status
        .get("expiresAt")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let scopes = status
        .get("scopes")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    println!("{} {}", out::heading("Remote:"), out::repo(&remote_name));
    println!("{} {kind}", out::heading("Token kind:"));
    println!("{} {subject}", out::heading("Subject:"));
    println!("{} {environment}", out::heading("Environment:"));
    println!("{} {expiry}", out::heading("Expires:"));
    println!("{} {scopes}", out::heading("Scopes:"));
    match forge_probe_error {
        Some(error) => println!(
            "{} {}",
            out::heading("Forge credentials:"),
            out::muted(format!(
                "unavailable ({error}); the sync login above is still valid"
            ))
        ),
        None => println!(
            "{} {connected} connected",
            out::heading("Forge credentials:")
        ),
    }
    Ok(())
}

fn describe_configured_remotes(config: &KnitConfig) -> String {
    if config.remotes.is_empty() {
        "none yet".to_string()
    } else {
        config
            .remotes
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Interactive remote selection: list the user-level remotes, then take a
/// name — an existing one to update or a brand-new one to add.
fn prompt_remote_name(config: &KnitConfig) -> Result<String> {
    if config.remotes.is_empty() {
        println!(
            "{}",
            out::muted("No sync remotes are configured yet (user-level config).")
        );
    } else {
        println!("Configured sync remotes (user-level):");
        for (name, remote) in &config.remotes {
            println!("  {} → {}", out::repo(name), display_url(&remote.url));
        }
    }
    loop {
        let answer =
            prompt_input("Remote name to set up or update (an existing name, or a new one): ")?;
        if require_nameable(&answer).is_ok() {
            return Ok(slugify(&answer));
        }
        println!("Remote name must contain at least one letter or digit.");
    }
}

/// Interactive endpoint entry for a remote that does not exist yet.
fn prompt_service_url(remote_name: &str) -> Result<String> {
    loop {
        let answer = prompt_input(&format!(
            "Service URL for `{remote_name}` (for example https://host.example): "
        ))?;
        match validate_service_url(&answer) {
            Ok(url) => return Ok(url),
            Err(error) => println!("{error}"),
        }
    }
}

fn prompt_input(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        bail!("Setup cancelled: input closed.");
    }
    Ok(input.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(url: &str, token: Option<&str>) -> KnitRemote {
        KnitRemote {
            url: url.to_owned(),
            token: token.map(str::to_owned),
        }
    }

    #[test]
    fn service_urls_must_be_real_hosts_without_credentials_or_extra_parts() {
        assert_eq!(
            validate_service_url("https://host.example").unwrap(),
            "https://host.example"
        );
        assert_eq!(
            validate_service_url("  http://localhost:4000/  ").unwrap(),
            "http://localhost:4000"
        );
        assert_eq!(
            validate_service_url("https://host.example/api/v1").unwrap(),
            "https://host.example/api/v1"
        );
        for bad in [
            "",
            "  ",
            "not a url",
            "ftp://host.example",
            "https://",
            "https://user:secret@host.example",
            "https://user@host.example",
            "https://host.example?token=SECRET",
            "https://host.example#fragment",
            "https://host.example/pa th",
        ] {
            let error = validate_service_url(bad).unwrap_err().to_string();
            assert!(!error.contains("SECRET"), "{error}");
            assert!(!error.contains("pa th"), "{error}");
        }
    }

    #[test]
    fn displayed_urls_never_carry_legacy_credentials_or_queries() {
        assert_eq!(display_url("https://host.example"), "https://host.example");
        assert_eq!(
            display_url("http://localhost:4000/api/v1"),
            "http://localhost:4000/api/v1"
        );
        for legacy in [
            "https://user:SECRET@host.example/route",
            "https://host.example/route?token=SECRET",
            "https://user@host.example#frag",
        ] {
            let shown = display_url(legacy);
            assert!(!shown.contains("SECRET"), "{shown}");
            assert!(!shown.contains("user"), "{shown}");
            assert!(!shown.contains('?'), "{shown}");
            assert!(!shown.contains('#'), "{shown}");
        }
        assert_eq!(display_url("::::"), "<invalid stored URL>");
    }

    #[test]
    fn remote_names_need_at_least_one_letter_or_digit() {
        // `slugify` falls back to a generic slug for punctuation-only input;
        // the raw-name check must reject that before it names a remote.
        for name in ["", "!!!", "---", "//", " "] {
            assert!(require_nameable(name).is_err(), "{name:?}");
        }
        for name in ["hosted", "Hosted2", "a", "host-ed_2"] {
            assert!(require_nameable(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn envelope_shape_decides_verification() {
        assert!(envelope_data_is_object(
            r#"{"data":{"tokenKind":"legacy","scopes":[]}}"#
        ));
        assert!(!envelope_data_is_object(r#"{"data":[]}"#));
        assert!(!envelope_data_is_object(r#"{"data":"legacy"}"#));
        assert!(!envelope_data_is_object(r#"{"errors":{"detail":"nope"}}"#));
        assert!(!envelope_data_is_object("not json"));
        assert!(!envelope_data_is_object(""));
    }

    #[test]
    fn concurrent_remote_change_is_refused_and_normal_update_preserves_siblings() {
        let mut config = KnitConfig::empty();
        config.remotes.insert(
            "other".into(),
            remote("https://other.example", Some("kept")),
        );
        config
            .remotes
            .insert("hosted".into(), remote("https://host.example", None));

        // Unchanged snapshot: update applies and leaves siblings alone.
        let mut updated = config.clone();
        apply_remote_update(
            &mut updated,
            "hosted",
            Some(("https://host.example", None)),
            "https://host.example",
            "synthetic-candidate",
        )
        .unwrap();
        assert_eq!(
            updated.remotes["hosted"].token.as_deref(),
            Some("synthetic-candidate")
        );
        assert_eq!(
            updated.remotes["other"].token.as_deref(),
            Some("kept"),
            "unrelated remotes edited concurrently must survive"
        );

        // Token changed underneath: refused.
        let mut raced = config.clone();
        raced.remotes.insert(
            "hosted".into(),
            remote("https://host.example", Some("someone-else")),
        );
        let error = apply_remote_update(
            &mut raced,
            "hosted",
            Some(("https://host.example", None)),
            "https://host.example",
            "synthetic-candidate",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("changed while the token was being entered"),
            "{error}"
        );
        assert_eq!(
            raced.remotes["hosted"].token.as_deref(),
            Some("someone-else")
        );

        // URL changed underneath: refused.
        let mut moved = config.clone();
        moved
            .remotes
            .insert("hosted".into(), remote("https://moved.example", None));
        assert!(apply_remote_update(
            &mut moved,
            "hosted",
            Some(("https://host.example", None)),
            "https://host.example",
            "synthetic-candidate",
        )
        .is_err());

        // Remote appeared while the token was entered: refused.
        let mut appeared = KnitConfig::empty();
        appeared.remotes.insert(
            "hosted".into(),
            remote("https://host.example", Some("early")),
        );
        assert!(apply_remote_update(
            &mut appeared,
            "hosted",
            None,
            "https://host.example",
            "synthetic-candidate",
        )
        .is_err());

        // Remote vanished: refused, not resurrected.
        let mut vanished = config.clone();
        vanished.remotes.remove("hosted");
        assert!(apply_remote_update(
            &mut vanished,
            "hosted",
            Some(("https://host.example", None)),
            "https://host.example",
            "synthetic-candidate",
        )
        .is_err());
        assert!(!vanished.remotes.contains_key("hosted"));
    }

    // ---------------------------------------------------------------------------
    // A minimal loopback HTTP server: verification must classify 2xx/401/403/
    // 5xx/redirect/transport without ever hanging, and only a 2xx envelope
    // with an object `data` may verify.
    // ---------------------------------------------------------------------------

    fn loopback_base(handle: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::sync::Arc::new(handle);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let handle = handle.clone();
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 4096];
                    let read = std::io::Read::read(&mut stream, &mut buffer).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let target = request.split_whitespace().nth(1).unwrap_or_default();
                    let (status, body) = handle(target);
                    let reason = match status {
                        200 => "OK",
                        302 => "Found",
                        401 => "Unauthorized",
                        403 => "Forbidden",
                        _ => "Server Error",
                    };
                    let extra = if status == 302 {
                        "location: /elsewhere\r\n"
                    } else {
                        ""
                    };
                    let _ = std::io::Write::write_all(
                        &mut stream,
                        format!(
                            "HTTP/1.1 {status} {reason}\r\ncontent-type: \
                             application/json\r\ncontent-length: {}\r\nconnection: \
                             close\r\n{extra}\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                });
            }
        });
        base
    }

    #[test]
    fn verification_classifies_responses_without_trusting_bodies() {
        let accepted = r#"{"data":{"tokenKind":"legacy","scopes":["bundle:read"]}}"#;
        for (body, expected_verified) in [
            (accepted.to_string(), true),
            // A 200 whose body is not the envelope shape is indeterminate.
            (r#"{"data":[1,2]}"#.to_string(), false),
            ("not json".to_string(), false),
        ] {
            let base = loopback_base(move |_| (200, body.clone()));
            match verify_remote_token(&base, "synthetic-candidate") {
                TokenVerification::Verified => assert!(expected_verified),
                TokenVerification::Rejected(status) => {
                    panic!("a 200 must never classify as rejected: {status}")
                }
                TokenVerification::Indeterminate(reason) => {
                    assert!(!expected_verified);
                    assert!(!reason.contains("synthetic-candidate"), "{reason}");
                }
            }
        }
        for status in [401u16, 403] {
            let base = loopback_base(move |_| (status, String::new()));
            match verify_remote_token(&base, "synthetic-candidate") {
                TokenVerification::Rejected(actual) => assert_eq!(actual, status),
                other => panic!("HTTP {status} must classify as rejected, got {other:?}"),
            }
        }
        let base = loopback_base(|_| (500, "oops".into()));
        match verify_remote_token(&base, "synthetic-candidate") {
            TokenVerification::Indeterminate(reason) => {
                assert!(reason.contains("500"), "{reason}");
                assert!(!reason.contains("invalid"), "{reason}");
            }
            other => panic!("a 500 must stay indeterminate, got {other:?}"),
        }
        // A redirect carrying a perfectly shaped envelope body must NOT
        // verify: redirects are never followed.
        let envelope = accepted.to_string();
        let base = loopback_base(move |_| (302, envelope.clone()));
        match verify_remote_token(&base, "synthetic-candidate") {
            TokenVerification::Indeterminate(reason) => {
                assert!(reason.contains("302"), "{reason}");
            }
            other => panic!("a redirect must stay indeterminate, got {other:?}"),
        }
    }

    #[test]
    fn verification_honors_the_api_v1_suffix_and_classifies_connection_failures() {
        let base = loopback_base(|target| {
            assert_eq!(target, "/api/v1/me/access-token", "target {target}");
            (200, r#"{"data":{"ok":true}}"#.into())
        });
        // An explicit /api/v1 suffix is honored as-is…
        match verify_remote_token(&format!("{base}/api/v1"), "synthetic-candidate") {
            TokenVerification::Verified => {}
            other => panic!("the /api/v1 suffix must be honored, got {other:?}"),
        }
        // …and a bare base gets /api/v1 appended.
        match verify_remote_token(&base, "synthetic-candidate") {
            TokenVerification::Verified => {}
            other => panic!("the /api/v1 suffix must be appended, got {other:?}"),
        }

        // A bound-then-dropped listener gives a refused connection.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        match verify_remote_token(&base, "synthetic-candidate") {
            TokenVerification::Indeterminate(reason) => {
                assert!(!reason.contains("synthetic-candidate"), "{reason}");
                assert!(!reason.contains("invalid"), "{reason}");
            }
            other => panic!("connection refusal must stay indeterminate, got {other:?}"),
        }
    }
}
