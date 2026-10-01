//! Read templates from the review destination, never from the contributor checkout.
use crate::model::RepoEntry;
use crate::providers::{self, PrTarget};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub(super) fn upstream_template(
    repo: &RepoEntry,
    base: &str,
    cwd: &Path,
) -> Result<Option<(String, String)>> {
    let remote = crate::contribution::destination(repo)
        .context("template lookup requires a repository URL")?;
    let forge = providers::for_repo(repo)?;
    let name = forge
        .repo_full_name(remote)
        .context("cannot identify upstream template repository")?;
    let mut target = PrTarget::explicit(cwd, &name);
    target.repo_remote = Some(remote.into());
    let encode = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    for path in [
        ".github/pull_request_template.md",
        ".github/PULL_REQUEST_TEMPLATE.md",
        "docs/pull_request_template.md",
        "docs/PULL_REQUEST_TEMPLATE.md",
    ] {
        let native = providers::target_credential(&target, forge.id())?.is_some()
            || !matches!(forge.id(), "github" | "gitlab")
            || (forge.id() == "github" && std::env::var("KNIT_GITHUB_API_TRANSPORT").is_ok());
        if native {
            let endpoint = match forge.id() {
                "github" => format!("repos/{name}/contents/{path}?ref={}", encode(base)),
                "gitlab" => format!(
                    "projects/{}/repository/files/{}/raw?ref={}",
                    encode(&name),
                    encode(path),
                    encode(base)
                ),
                "forgejo" => format!("repos/{name}/raw/{path}?ref={}", encode(base)),
                "bitbucket" => format!("repositories/{name}/src/{}/{path}", encode(base)),
                other => bail!("unsupported template provider {other}"),
            };
            if let Some(text) = native_template(&target, forge.id(), &endpoint)? {
                return Ok(Some((format!("upstream:{name}/{path}@{base}"), text)));
            }
            continue;
        }
        let (bin,mut args): (&str,Vec<String>) = match forge.id() {
            "github" => ("gh",vec!["api".into(),"--method".into(),"GET".into(),format!("repos/{name}/contents/{path}?ref={}",encode(base)),"-H".into(),"Accept: application/vnd.github.raw+json".into()]),
            "gitlab" => ("glab",vec!["api".into(),"--method".into(),"GET".into(),format!("projects/{}/repository/files/{}/raw?ref={}",encode(&name),encode(path),encode(base))]),
            other => bail!("upstream-template lookup is not supported for {other}; configure a body file or fallback=knit"),
        };
        args.push("--hostname".into());
        args.push(crate::auth::remote_target(remote)?.0);
        match providers::cli_output(bin, &target, args, None) {
            Ok(text) => return Ok(Some((format!("upstream:{name}/{path}@{base}"), text))),
            Err(error) if format!("{error:#}").contains("404") => {}
            Err(error) => return Err(error).context("read upstream PR template"),
        }
    }
    Ok(None)
}

fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}
fn base64(value: &str) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in value.as_bytes().chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        result.push(ABC[((n >> 18) & 63) as usize] as char);
        result.push(ABC[((n >> 12) & 63) as usize] as char);
        result.push(if chunk.len() > 1 {
            ABC[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            ABC[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    result
}
fn native_template(target: &PrTarget, provider: &str, endpoint: &str) -> Result<Option<String>> {
    let credential = providers::target_credential(target, provider)?;
    let host = crate::auth::remote_target(
        target
            .repo_remote
            .as_deref()
            .context("missing template remote")?,
    )?
    .0;
    let (env_base, default_base, tokens): (&str, String, &[&str]) = match provider {
        "github" => (
            "KNIT_GITHUB_API_BASE",
            if host == "github.com" {
                "https://api.github.com".into()
            } else {
                format!("https://{host}/api/v3")
            },
            &["GH_TOKEN", "GITHUB_TOKEN"],
        ),
        "gitlab" => (
            "KNIT_GITLAB_API_BASE",
            format!("https://{host}/api/v4"),
            &["GITLAB_TOKEN", "GLAB_TOKEN"],
        ),
        "forgejo" => (
            "KNIT_FORGEJO_API_BASE",
            format!("https://{host}/api/v1"),
            &["KNIT_FORGEJO_TOKEN", "CODEBERG_TOKEN", "GITEA_TOKEN"],
        ),
        "bitbucket" => (
            "KNIT_BITBUCKET_API_BASE",
            "https://api.bitbucket.org/2.0".into(),
            &["KNIT_BITBUCKET_ACCESS_TOKEN"],
        ),
        _ => bail!("unsupported template provider {provider}"),
    };
    let api = if let Some(c) = &credential {
        providers::bound_api_base(c)?
    } else {
        first_env(&[env_base]).unwrap_or(default_base)
    };
    let token = credential
        .as_ref()
        .map(|c| c.token.clone())
        .or_else(|| first_env(tokens));
    let authorization = if provider == "bitbucket" {
        if let Some(c) = &credential {
            match c.token_type.as_deref() {
                Some("access_token") => Some(format!("Bearer {}", c.token)),
                Some("atlassian_api_token")
                    if !crate::auth::is_bitbucket_account_email(&c.username) =>
                {
                    bail!("Bitbucket API token requires an account email")
                }
                _ if !c.username.is_empty() => Some(format!(
                    "Basic {}",
                    base64(&format!("{}:{}", c.username, c.token))
                )),
                _ => Some(format!("Bearer {}", c.token)),
            }
        } else if let Some(token) = token {
            Some(format!("Bearer {token}"))
        } else if let (Some(email), Some(token)) = (
            first_env(&["KNIT_BITBUCKET_EMAIL"]),
            first_env(&["KNIT_BITBUCKET_API_TOKEN"]),
        ) {
            Some(format!("Basic {}", base64(&format!("{email}:{token}"))))
        } else {
            None
        }
    } else {
        token.map(|t| {
            format!(
                "{} {t}",
                if provider == "forgejo" {
                    "token"
                } else {
                    "Bearer"
                }
            )
        })
    };
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(std::time::Duration::from_secs(20))
        .build();
    let mut request = agent
        .get(&format!("{}/{}", api.trim_end_matches('/'), endpoint))
        .set("User-Agent", "knit");
    if provider == "github" {
        request = request.set("Accept", "application/vnd.github.raw+json");
    }
    if let Some(auth) = &authorization {
        request = request.set("Authorization", auth);
    }
    match request.call() {
        Ok(response) => Ok(Some(
            response
                .into_string()
                .context("read upstream template response")?,
        )),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(ureq::Error::Status(code, _)) => bail!("upstream template lookup returned HTTP {code}"),
        Err(_) => bail!("upstream template transport failed for {provider}"),
    }
}
