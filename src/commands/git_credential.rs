//! Provider-neutral Git credential helper backed by a named Knit remote.

use crate::cli::GitCredentialOperation;
use crate::commands::remote::{normalize_git_target, request_forge_credential, VendAttempt};
use crate::ids::slugify;
use crate::model::KnitRemote;
use crate::store::load_global_config;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{self, Read};

const MAX_CREDENTIAL_INPUT: u64 = 64 * 1024;

pub fn run_git_credential_helper(
    remote_name: &str,
    operation: GitCredentialOperation,
) -> Result<()> {
    let input = read_input()?;
    if !matches!(operation, GitCredentialOperation::Get) {
        return Ok(());
    }

    let Some(protocol) = input.get("protocol") else {
        return Ok(());
    };
    let Some(host) = input.get("host") else {
        return Ok(());
    };
    let Some(host) = normalize_git_target(protocol, host) else {
        return Ok(());
    };

    // Credential-bearing remotes are a user security boundary. Never let a
    // repository-controlled workspace config replace their URL or token.
    let config = load_global_config()?;
    let remote_name = slugify(remote_name);
    let remote = config
        .remotes
        .get(&remote_name)
        .with_context(|| format!("No remote named `{remote_name}`."))?;
    let token = resolve_token(&remote_name, remote)?;
    let path = input.get("path").map(String::as_str);

    match request_forge_credential(remote, &token, "https", &host, path)? {
        VendAttempt::Credential(credential) => {
            println!("username={}", credential.username);
            println!("password={}", credential.password);
            println!();
        }
        VendAttempt::NoCredential => {}
    }
    Ok(())
}

fn read_input() -> Result<BTreeMap<String, String>> {
    let mut bytes = Vec::new();
    io::stdin()
        .take(MAX_CREDENTIAL_INPUT + 1)
        .read_to_end(&mut bytes)
        .context("failed to read Git credential request")?;
    if bytes.len() as u64 > MAX_CREDENTIAL_INPUT {
        bail!("Git credential request exceeds {MAX_CREDENTIAL_INPUT} bytes");
    }
    let input = String::from_utf8(bytes).context("Git credential request is not UTF-8")?;
    let mut fields = BTreeMap::new();
    for line in input.lines().take_while(|line| !line.is_empty()) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if matches!(key, "protocol" | "host" | "path") {
            fields.insert(key.to_string(), value.to_string());
        }
    }
    Ok(fields)
}

fn resolve_token(name: &str, remote: &KnitRemote) -> Result<String> {
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
        .or_else(|| remote.token.clone())
        .context("No token configured for the selected Knit remote.")
}
