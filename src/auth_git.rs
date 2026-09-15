//! Per-invocation Git credentials. Repository bindings never alter Git config.
use crate::auth::{self, ResolvedCredential};
use crate::cli::GitCredentialOperation;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

/// Add only credential references to the command line. The token is loaded by
/// the helper subprocess after Git supplies a matching HTTPS repository path.
pub fn configure(
    cwd: &Path,
    args: &[OsString],
    command: &mut Command,
) -> Result<Vec<ResolvedCredential>> {
    let Some((operation, targets)) = network_targets(cwd, args)? else {
        return Ok(Vec::new());
    };
    if changes_repository(args) {
        let registry = auth::load()?;
        if let Ok((root, project)) = auth::project_context(cwd, None) {
            if registry
                .projects
                .get(&auth::project_key(&root, &project.id)?)
                .is_some_and(|bindings| !bindings.is_empty())
            {
                bail!("Git repository location overrides are unavailable with project credentials; run Knit from the intended project checkout instead");
            }
        }
    }
    let mut selected = Vec::new();
    for target in targets {
        let urls = remote_urls(cwd, &target, operation == "push");
        for remote in urls {
            let Some(credential) = auth::resolve(cwd, Some(&remote))? else {
                continue;
            };
            let (host, path) = exact_target(&remote)?;
            if selected.is_empty() {
                // Reset inherited helpers for this child, including requests
                // from recursive submodule fetches. A project binding must not
                // fall back to a broader ambient credential on another path.
                command.args([
                    "-c",
                    "credential.helper=",
                    "-c",
                    "credential.useHttpPath=true",
                ]);
                command.env("GIT_TERMINAL_PROMPT", "0");
                command.env("GIT_ASKPASS", "");
                command.env("SSH_ASKPASS", "");
                for (name, _) in std::env::vars_os() {
                    if name.to_string_lossy().starts_with("GIT_TRACE") {
                        command.env_remove(name);
                    }
                }
                command.env_remove("GIT_CURL_VERBOSE");
                command.env("GIT_TRACE_REDACT", "1");
                command.args([
                    "-c",
                    "core.askPass=",
                    "-c",
                    "http.followRedirects=false",
                    "-c",
                    "http.extraHeader=",
                ]);
            }
            let executable = std::env::current_exe().context("locating Knit credential helper")?;
            let helper = format!(
                "!{} auth git-credential --credential {} --host {} --path {}",
                shell_quote(&executable.to_string_lossy()),
                shell_quote(&credential.name),
                shell_quote(&host),
                shell_quote(&path),
            );
            configure_http_auth(command, &host, &path);
            // Send the assigned credential on the first request as well. This
            // prevents libcurl's .netrc from authenticating first on older Git
            // versions that do not understand http.proactiveAuth. --config-env
            // keeps the header out of argv and persistent Git configuration.
            let variable = format!("KNIT_GIT_AUTH_HEADER_{}", selected.len());
            let encoded =
                base64(format!("{}:{}", credential.git_username(), credential.token).as_bytes());
            command.env(&variable, format!("Authorization: Basic {encoded}"));
            command.arg(format!(
                "--config-env=http.https://{host}/{path}.extraHeader={variable}"
            ));
            let scope = format!("credential.https://{host}/{path}.helper");
            command.arg("-c").arg(format!("{scope}="));
            command.arg("-c").arg(format!("{scope}={helper}"));
            // The selected credential must ride the HTTPS transport: an SSH
            // origin is rewritten to HTTPS instead of bypassing the token
            // through the user's SSH agent, and an HTTPS origin gets the same
            // exact identity rewrite so an inherited broad rewrite (a global
            // SSH workaround like url.ssh://git@host/.insteadOf=https://host/)
            // cannot divert this exact URL back to SSH. Git resolves insteadOf
            // by longest matching prefix, so the exact key always outranks the
            // inherited one for this invocation only.
            command
                .arg("-c")
                .arg(format!("url.https://{host}/{path}.insteadOf={remote}"));
            selected.push(credential);
        }
    }
    Ok(selected)
}

pub fn redact(credentials: &[ResolvedCredential], text: &str) -> String {
    credentials
        .iter()
        .fold(text.to_owned(), |text, credential| {
            let encoded =
                base64(format!("{}:{}", credential.git_username(), credential.token).as_bytes());
            credential.redact(&text).replace(&encoded, "[REDACTED]")
        })
}

// Git's HTTP URL matching gives scoped settings precedence over global -c
// values. Reset at the exact repository URL so inherited owner/host settings
// cannot authenticate with an unrelated header or cookie before our helper.
fn configure_http_auth(command: &mut Command, host: &str, path: &str) {
    for (key, value) in [
        ("extraHeader", ""),
        ("cookieFile", ""),
        ("saveCookies", "false"),
        ("followRedirects", "false"),
        ("emptyAuth", "false"),
        ("proactiveAuth", "basic"),
    ] {
        command
            .arg("-c")
            .arg(format!("http.https://{host}/{path}.{key}={value}"));
    }
}

fn changes_repository(args: &[OsString]) -> bool {
    let mut index = 0;
    while let Some(arg) = args.get(index).and_then(|arg| arg.to_str()) {
        if !arg.starts_with('-') {
            break;
        }
        if arg == "-C"
            || arg.starts_with("-C")
            || arg == "--git-dir"
            || arg.starts_with("--git-dir=")
            || arg == "--work-tree"
            || arg.starts_with("--work-tree=")
        {
            return true;
        }
        index += if matches!(arg, "-c" | "--namespace" | "--config-env") {
            2
        } else {
            1
        };
    }
    false
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        result.push(ALPHABET[(a >> 2) as usize] as char);
        result.push(ALPHABET[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        result.push(if chunk.len() > 1 {
            ALPHABET[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            ALPHABET[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    result
}

pub fn credential_helper(
    name: &str,
    host: &str,
    path: &str,
    operation: GitCredentialOperation,
) -> Result<()> {
    if !matches!(operation, GitCredentialOperation::Get) {
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut fields = BTreeMap::new();
    let mut total = 0;
    for line in stdin.lock().lines() {
        let line = line?;
        total += line.len();
        if total > 64 * 1024 {
            bail!("Git credential request exceeds limit");
        }
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            // Git may repeat array metadata such as capability[] and wwwauth[].
            // Ignore attributes we do not use, but reject ambiguous scope scalars.
            if !matches!(key, "protocol" | "host" | "path" | "url") {
                continue;
            }
            if fields.insert(key.to_owned(), value.to_owned()).is_some() {
                bail!("duplicate field in Git credential request");
            }
        }
    }
    // Do not load the secret for another host, path, or protocol. Multiple
    // helpers may be installed for `fetch --all`, so a mismatch emits nothing.
    if fields.get("protocol").map(String::as_str) != Some("https")
        || fields.get("host").map(String::as_str) != Some(host)
        || fields.get("path").map(String::as_str) != Some(path)
        || fields.contains_key("url")
    {
        return Ok(());
    }
    let credential = auth::credential(name)?;
    if credential.host != host {
        bail!("credential host does not match Git request");
    }
    let username = credential.git_username();
    if username.contains(['\n', '\r']) || credential.token.contains(['\n', '\r']) {
        bail!("invalid Git credential value");
    }
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(
        stdout,
        "username={username}\npassword={}\n",
        credential.token
    )?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Preserve the actual Git path, including `.git`, for credential matching.
fn exact_target(remote: &str) -> Result<(String, String)> {
    let url = if remote.contains("://") {
        url::Url::parse(remote)?
    } else {
        let (authority, path) = remote.split_once(':').context("invalid forge remote")?;
        url::Url::parse(&format!("ssh://{authority}/{path}"))?
    };
    if !matches!(url.scheme(), "https" | "ssh") {
        bail!("project credentials require an HTTPS or SSH forge remote");
    }
    if url.query().is_some() || url.fragment().is_some() || url.password().is_some() {
        bail!("forge remote must not contain a password, query, or fragment");
    }
    let host = url.host_str().context("forge remote has no host")?;
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let path = url.path().trim_start_matches('/');
    if path.is_empty() || path.contains(['\n', '\r']) {
        bail!("forge remote has no repository path");
    }
    Ok((host, path.to_owned()))
}

fn raw_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn remote_urls(cwd: &Path, target: &str, push: bool) -> Vec<String> {
    let args = if push {
        vec!["remote", "get-url", "--push", "--all", target]
    } else {
        vec!["remote", "get-url", target]
    };
    raw_git(cwd, &args)
        .map(|urls| urls.lines().map(str::to_owned).collect())
        .unwrap_or_else(|| vec![target.to_owned()])
}

fn default_remote(cwd: &Path, push: bool) -> String {
    let branch = raw_git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    if push {
        if let Some(remote) = branch
            .as_ref()
            .and_then(|branch| {
                raw_git(
                    cwd,
                    &["config", "--get", &format!("branch.{branch}.pushRemote")],
                )
            })
            .or_else(|| raw_git(cwd, &["config", "--get", "remote.pushDefault"]))
        {
            return remote;
        }
    }
    branch
        .and_then(|branch| {
            raw_git(
                cwd,
                &["config", "--get", &format!("branch.{branch}.remote")],
            )
        })
        .unwrap_or_else(|| "origin".to_owned())
}

fn all_remotes(cwd: &Path) -> Vec<String> {
    raw_git(cwd, &["remote"])
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Git remote groups expand once into configured remote names. Resolve all
/// members before constructing a network command, so one missing assignment
/// prevents the entire operation rather than falling back to ambient auth.
fn expand_remote_groups(cwd: &Path, names: Vec<String>, single_fetch: bool) -> Result<Vec<String>> {
    let mut targets = Vec::new();
    for name in names {
        let members = match raw_git(cwd, &["config", "--get-all", &format!("remotes.{name}")]) {
            Some(group) => {
                let members: Vec<_> = group.split_whitespace().map(str::to_owned).collect();
                if members.is_empty() {
                    bail!("Git remote group has no members");
                }
                // Plain fetch only enters Git's group execution path when
                // there are multiple entries (before deduplication). For a
                // singleton it fetches the original operand as a remote/URL.
                if single_fetch && members.len() == 1 {
                    vec![name]
                } else {
                    for member in &members {
                        if raw_git(cwd, &["remote", "get-url", member]).is_none() {
                            bail!("Git remote group contains an unconfigured remote; refusing unauthenticated fallback");
                        }
                    }
                    members
                }
            }
            None => vec![name],
        };
        for member in members {
            if !targets.contains(&member) {
                targets.push(member);
            }
        }
    }
    Ok(targets)
}

/// Extract the remote operand without confusing option values or refspecs for
/// repository names. All network forms used by Knit pass explicit operands.
fn network_targets(cwd: &Path, args: &[OsString]) -> Result<Option<(String, Vec<String>)>> {
    let args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut start = 0;
    while start < args.len() && args[start].starts_with('-') {
        match args[start].as_str() {
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" => start += 2,
            _ => start += 1,
        }
    }
    let Some(operation) = args.get(start) else {
        return Ok(None);
    };
    if !matches!(
        operation.as_str(),
        "fetch" | "push" | "pull" | "ls-remote" | "clone" | "remote"
    ) {
        return Ok(None);
    }
    let mut rest = &args[start + 1..];
    if operation == "remote" {
        if rest.first().map(String::as_str) != Some("update") {
            return Ok(None);
        }
        rest = &rest[1..];
        let names = rest
            .iter()
            .filter(|arg| !arg.starts_with('-'))
            .cloned()
            .collect::<Vec<_>>();
        let targets = if names.is_empty() {
            all_remotes(cwd)
        } else {
            expand_remote_groups(cwd, names, false)?
        };
        return Ok(Some((operation.clone(), targets)));
    }
    if operation == "fetch" && rest.iter().any(|arg| arg == "--all") {
        return Ok(Some((operation.clone(), all_remotes(cwd))));
    }
    let mut operands = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let arg = &rest[index];
        if arg == "--" {
            operands.extend_from_slice(&rest[index + 1..]);
            break;
        }
        if arg.starts_with('-') {
            if matches!(
                arg.as_str(),
                "--depth"
                    | "--deepen"
                    | "--shallow-since"
                    | "--shallow-exclude"
                    | "--upload-pack"
                    | "--receive-pack"
                    | "--server-option"
                    | "-o"
                    | "--repo"
                    | "--branch"
                    | "-b"
                    | "--origin"
                    | "--template"
                    | "--reference"
                    | "--reference-if-able"
                    | "--separate-git-dir"
                    | "--filter"
                    | "--jobs"
                    | "-j"
                    | "--negotiation-tip"
                    | "--refmap"
                    | "--recurse-submodules-default"
                    | "--push-option"
                    | "--exec"
            ) {
                if arg == "--repo" && operation == "push" {
                    if let Some(repo) = rest.get(index + 1) {
                        return Ok(Some((operation.clone(), vec![repo.clone()])));
                    }
                }
                index += 1;
            } else if operation == "push" && arg.starts_with("--repo=") {
                return Ok(Some((operation.clone(), vec![arg[7..].to_owned()])));
            }
        } else {
            operands.push(arg.clone());
        }
        index += 1;
    }
    let has_operands = !operands.is_empty();
    let targets = if operation == "fetch" && rest.iter().any(|arg| arg == "--multiple") {
        operands
    } else {
        vec![operands
            .into_iter()
            .next()
            .unwrap_or_else(|| default_remote(cwd, operation == "push"))]
    };
    let targets = if operation == "fetch" && has_operands {
        expand_remote_groups(cwd, targets, !rest.iter().any(|arg| arg == "--multiple"))?
    } else {
        targets
    };
    Ok(Some((operation.clone(), targets)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_http_settings_override_host_and_owner_settings() {
        for (key, expected) in [
            ("extraHeader", ""),
            ("cookieFile", ""),
            ("saveCookies", "false"),
            ("followRedirects", "false"),
            ("emptyAuth", "false"),
            ("proactiveAuth", "basic"),
        ] {
            let mut command = Command::new("git");
            command
                .current_dir(std::env::temp_dir())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    std::env::temp_dir().join("knit-nonexistent-global-config"),
                )
                .args([
                    "-c",
                    &format!("http.{key}=global"),
                    "-c",
                    &format!("http.https://github.com/.{key}=host"),
                    "-c",
                    &format!("http.https://github.com/org/.{key}=owner"),
                    "-c",
                    &format!("http.https://github.com/org/repo.git.{key}=repository"),
                ]);
            configure_http_auth(&mut command, "github.com", "org/repo.git");
            let output = command
                .args([
                    "config",
                    "--get-urlmatch",
                    &format!("http.{key}"),
                    "https://github.com/org/repo.git",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            // extraHeader is multi-valued: the trailing empty value resets
            // any earlier exact-match values when Git consumes this list.
            let value = String::from_utf8(output.stdout).unwrap();
            assert_eq!(value.lines().last(), Some(expected));
            if key != "extraHeader" {
                assert_eq!(value.trim_end(), expected);
            }
        }
    }

    #[test]
    fn redact_git_basic_auth_and_detect_location_overrides() {
        let credential = ResolvedCredential {
            name: "work".into(),
            provider: "github".into(),
            host: "github.com".into(),
            username: "".into(),
            token: "scoped-test-secret".into(),
        };
        assert_eq!(
            redact(
                &[credential],
                "Authorization: Basic eC1hY2Nlc3MtdG9rZW46c2NvcGVkLXRlc3Qtc2VjcmV0"
            ),
            "Authorization: Basic [REDACTED]"
        );
        for args in [
            vec!["-C", "/other", "fetch"],
            vec!["--git-dir=/other/.git", "fetch"],
            vec!["-c", "color.ui=false", "--work-tree", "/other", "push"],
        ] {
            assert!(changes_repository(
                &args.into_iter().map(OsString::from).collect::<Vec<_>>()
            ));
        }
        assert!(!changes_repository(&["fetch".into(), "-C".into()]));
    }

    #[test]
    fn exact_target_preserves_git_suffix_and_quotes_shell_values() {
        assert_eq!(
            exact_target("git@github.com:org/repo.git").unwrap(),
            ("github.com".into(), "org/repo.git".into())
        );
        assert_eq!(
            exact_target("https://github.com/org/repo.git").unwrap(),
            ("github.com".into(), "org/repo.git".into())
        );
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn exact_identity_rewrite_outranks_inherited_ssh_workaround() {
        // What configure adds for an HTTPS target whose credential resolved:
        // an exact url.https/<host>/<path>.insteadOf of the URL itself.
        let identity =
            "url.https://github.com/org/repo.git.insteadOf=https://github.com/org/repo.git";
        let inherited = "url.ssh://git@github.com/.insteadOf=https://github.com/";
        let resolved_url = |config: &[&str]| {
            let mut command = Command::new("git");
            command
                .current_dir(std::env::temp_dir())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    std::env::temp_dir().join("knit-nonexistent-global-config"),
                );
            for entry in config {
                command.arg("-c").arg(entry);
            }
            let output = command
                .args(["ls-remote", "--get-url", "https://github.com/org/repo.git"])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        // Without the identity key, the inherited SSH rewrite wins and the
        // URL leaves HTTPS (bypassing the selected credential's transport).
        assert_eq!(
            resolved_url(&[inherited]),
            "ssh://git@github.com/org/repo.git"
        );
        // With it, the exact match outranks the broader inherited prefix.
        assert_eq!(
            resolved_url(&[inherited, identity]),
            "https://github.com/org/repo.git"
        );
    }
    #[test]
    fn network_operands_follow_options_and_explicit_remote() {
        let cwd = Path::new(".");
        for (args, target) in [
            (
                vec!["fetch", "--depth", "1", "upstream", "main"],
                "upstream",
            ),
            (vec!["push", "--set-upstream", "other", "HEAD"], "other"),
            (
                vec![
                    "ls-remote",
                    "--symref",
                    "https://github.com/org/repo",
                    "HEAD",
                ],
                "https://github.com/org/repo",
            ),
            (
                vec![
                    "clone",
                    "--branch",
                    "main",
                    "git@github.com:org/repo.git",
                    "directory",
                ],
                "git@github.com:org/repo.git",
            ),
            (vec!["push", "--repo=upstream", "HEAD"], "upstream"),
        ] {
            let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
            assert_eq!(
                network_targets(cwd, &args).unwrap().unwrap().1,
                vec![target]
            );
        }
        assert!(
            network_targets(cwd, &["remote".into(), "get-url".into(), "origin".into()])
                .unwrap()
                .is_none()
        );
    }
}
