//! Transport for GitHub API calls: either `gh api` (default) or, when
//! `KNIT_GITHUB_API_TRANSPORT` selects it, a built-in HTTP client intended
//! for non-interactive runtimes where provider CLI prompts, credential
//! stores, or default IPv6 routing can hang simple GitHub I/O.

use super::CLI;
use crate::providers::{cli_output, PrTarget};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;

pub(super) fn github_api_output(
    target: &PrTarget,
    method: &str,
    endpoint: &str,
    body: Option<&str>,
) -> Result<String> {
    if use_native_github_api(target) {
        return native_github_api_output(method, endpoint, body);
    }

    let mut args = vec![OsString::from("api")];
    if method != "GET" {
        args.push(OsString::from("--method"));
        args.push(OsString::from(method));
    }
    args.push(OsString::from(endpoint));
    if body.is_some() {
        args.push(OsString::from("--input"));
        args.push(OsString::from("-"));
    }
    cli_output(CLI, &target.cwd, args, body)
}

pub(super) fn use_native_github_api(target: &PrTarget) -> bool {
    target.repo_full_name.is_some()
        && std::env::var("KNIT_GITHUB_API_TRANSPORT")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    // "curl"/"curl-ipv4"/"ipv4" are the historical values from
                    // when this transport shelled out to `curl --ipv4`; they
                    // keep selecting the same (now native) IPv4-first transport.
                    "curl" | "curl-ipv4" | "ipv4" | "native" | "api"
                )
            })
            .unwrap_or(false)
}

fn github_api_base() -> String {
    std::env::var("KNIT_GITHUB_API_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
}

/// Resolve hostnames preferring IPv4 addresses. This transport exists for
/// non-interactive runtimes where default IPv6 routing can hang simple GitHub
/// I/O, so v6 addresses are only used when no v4 address resolves.
fn ipv4_first_resolver(netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    let all: Vec<std::net::SocketAddr> = netloc.to_socket_addrs()?.collect();
    let v4: Vec<std::net::SocketAddr> = all
        .iter()
        .copied()
        .filter(std::net::SocketAddr::is_ipv4)
        .collect();
    Ok(if v4.is_empty() { all } else { v4 })
}

pub(super) fn native_github_api_output(
    method: &str,
    endpoint: &str,
    body: Option<&str>,
) -> Result<String> {
    let token = github_api_token()
        .context("KNIT_GITHUB_API_TRANSPORT requires GH_TOKEN or GITHUB_TOKEN")?;
    let url = format!("{}/{}", github_api_base(), endpoint.trim_start_matches('/'));
    let operation = format!("{method} /{}", endpoint.trim_start_matches('/'));

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(20))
        .resolver(ipv4_first_resolver as fn(&str) -> std::io::Result<Vec<std::net::SocketAddr>>)
        .build();
    let mut request = agent
        .request(method, &url)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "knit")
        .set("Authorization", &format!("Bearer {token}"));
    if body.is_some() {
        request = request.set("Content-Type", "application/json");
    }
    let result = match body {
        Some(input) => request.send_string(input),
        None => request.call(),
    };

    match result {
        Ok(response) => {
            let text = response
                .into_string()
                .with_context(|| format!("failed to read GitHub API response for {operation}"))?;
            Ok(text.trim_end().to_string())
        }
        Err(ureq::Error::Status(status, response)) => {
            let detail = response.into_string().unwrap_or_default();
            let detail = detail.trim();
            if status == 401 || looks_like_github_auth_failure(detail) {
                bail!(
                    "GitHub API request failed during {operation}: HTTP {status}: {detail}\nHint: GitHub rejected GH_TOKEN/GITHUB_TOKEN. Replace the saved GitHub credential with an active token that can access this repository, then retry."
                );
            }
            bail!("GitHub API request failed during {operation}: HTTP {status}: {detail}");
        }
        Err(ureq::Error::Transport(transport)) => {
            bail!("GitHub API request failed during {operation}: {transport}")
        }
    }
}

fn github_api_token() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"].into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn looks_like_github_auth_failure(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("bad credentials") || lower.contains("unauthorized")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_auth_failure_detects_bad_credentials() {
        assert!(looks_like_github_auth_failure(
            "{\"message\":\"Bad credentials\"}"
        ));
        assert!(!looks_like_github_auth_failure(
            "{\"message\":\"Not Found\"}"
        ));
    }

    #[test]
    fn ipv4_first_resolver_prefers_v4_addresses() {
        let addrs = ipv4_first_resolver("localhost:80").unwrap();
        assert!(!addrs.is_empty());
        // When any IPv4 address resolves, only IPv4 addresses are returned.
        if addrs.iter().any(std::net::SocketAddr::is_ipv4) {
            assert!(addrs.iter().all(std::net::SocketAddr::is_ipv4));
        }
    }
}
