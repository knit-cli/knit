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

/// A global-config helper value written by any knit binary for any remote name.
pub(crate) fn is_knit_helper(value: &str) -> bool {
    value.starts_with('!') && value.contains(HELPER_MARKER)
}

fn is_knit_helper_for(value: &str, remote_name: &str) -> bool {
    is_knit_helper(value)
        && value.ends_with(&format!("{HELPER_MARKER}{}", shell_quote(remote_name)))
}

fn helper_command(remote_name: &str) -> Result<String> {
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

/// The remote's connected forge hosts, exact-HTTPS-host validated on our side
/// regardless of what the server sent.
fn connected_forge_hosts(remote: &KnitRemote, token: &str) -> Result<BTreeSet<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
