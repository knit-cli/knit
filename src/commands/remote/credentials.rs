//! Forge credential brokering. The remote's endpoint returns a credential
//! only to `knit git-credential`, which speaks Git's credential protocol on
//! stdout — raw forge secrets never cross argv, environment variables, Git
//! config, or disk. Persistent helper installation lives in
//! `super::helpers`; clone and fetch rely on those installed helpers.

use super::client::request;
use crate::model::KnitRemote;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Appended to a repo's failure when HTTPS access was refused: the fix is on
/// the remote (connect a forge account) or outside knit (SSH credentials).
pub(super) const NO_ACCESS_HINT: &str =
    "no HTTPS git access: connect your forge account on the remote or configure SSH credentials";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VendedCredential {
    pub(crate) username: String,
    pub(crate) password: String,
}

/// Result of asking the server for a credential: either one to answer Git
/// with, or an authoritative absence/unsupported host response.
pub(crate) enum VendAttempt {
    Credential(VendedCredential),
    NoCredential,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
