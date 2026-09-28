//! Recover a rejected Bitbucket host default using verified ordinary Git access.
//! Only read/clone authentication is probed; explicit credential selections and
//! unrelated network failures never enter this path.
use crate::{auth, auth_git};
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) fn authentication_failure(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "authentication failed",
        "access denied",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "invalid username or password",
        "http 401",
        "http 403",
        "returned error: 401",
        "returned error: 403",
        "requires environment variable",
        "has no saved token",
    ]
    .iter()
    .any(|marker| detail.contains(marker))
}

pub(crate) fn ssh_url(remote: &str) -> Option<String> {
    let (host, path) = auth::remote_target(remote).ok()?;
    if host != "bitbucket.org"
        || path.is_empty()
        || path.contains(['?', '#', '\\'])
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(format!("ssh://git@bitbucket.org/{path}.git"))
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

fn owns_include(cwd: &Path) -> bool {
    raw_git(cwd, &["rev-parse", "--absolute-git-dir"])
        .is_some_and(|path| Path::new(&path).join("knit-credentials.inc").exists())
}

/// Preserve the user's helpers, headers, SSH command and TLS settings. Only
/// prompting/tracing are suppressed; no saved Knit secret enters this child.
pub(crate) fn configure_native(cwd: &Path, command: &mut Command, ssh: bool) {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GIT_TRACE_REDACT", "1")
        .arg("-c")
        .arg("core.askPass=");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_TRACE") || name == "GIT_CURL_VERBOSE" {
            command.env_remove(name);
        }
    }
    command.env("GIT_TRACE_REDACT", "1");
    if ssh {
        let ssh_command = std::env::var("GIT_SSH_COMMAND")
            .ok()
            .or_else(|| raw_git(cwd, &["config", "--get", "core.sshCommand"]))
            .or_else(|| {
                std::env::var("GIT_SSH")
                    .ok()
                    .map(|path| format!("'{}'", path.replace('\'', "'\\''")))
            })
            .unwrap_or_else(|| "ssh".to_owned());
        let variant = std::env::var("GIT_SSH_VARIANT")
            .ok()
            .or_else(|| raw_git(cwd, &["config", "--get", "ssh.variant"]));
        if variant
            .as_deref()
            .is_none_or(|value| matches!(value, "ssh" | "auto"))
        {
            command.env(
                "GIT_SSH_COMMAND",
                format!("{ssh_command} -oBatchMode=yes -oConnectTimeout=10"),
            );
        }
    }
}

enum Probe {
    Success,
    AuthenticationFailure,
    OtherFailure,
}

fn probe(cwd: &Path, remote: &str, ssh: bool) -> Result<Probe> {
    let mut command = Command::new("git");
    configure_native(cwd, &mut command, ssh);
    command
        .args(["ls-remote", "--", remote, "HEAD"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .context("starting ordinary Git authentication probe")?;
    let mut stderr = child.stderr.take().expect("piped stderr");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stderr = reader.join().unwrap_or_default();
    Ok(match status {
        Some(status) if status.success() => Probe::Success,
        Some(_) if authentication_failure(&String::from_utf8_lossy(&stderr)) => {
            Probe::AuthenticationFailure
        }
        _ => Probe::OtherFailure,
    })
}

/// Verify an alternative before recording it. The caller retries its original
/// operation at most once; a remembered route prevents recursive repair.
pub(crate) fn recover(cwd: &Path, args: &[OsString], failure: &str) -> Result<bool> {
    if !authentication_failure(failure) {
        return Ok(false);
    }
    let Some((operation, targets)) = auth_git::network_targets(cwd, args)? else {
        return Ok(false);
    };
    if !matches!(operation.as_str(), "clone" | "ls-remote" | "fetch") || targets.len() != 1 {
        return Ok(false);
    }
    let urls = auth_git::remote_urls(cwd, &targets[0], false);
    if urls.len() != 1 {
        return Ok(false);
    }
    let remote = &urls[0];
    let Some(credential) = auth::bitbucket_host_default(cwd, remote)? else {
        return Ok(false);
    };
    if auth::git_fallback(cwd, remote)?.is_some() {
        // A previously working native route can stop working after a key is
        // removed or the saved token is rotated. Give the current default one
        // fresh attempt instead of pinning a broken preference indefinitely.
        auth::clear_git_fallback(cwd, remote)?;
        if raw_git(cwd, &["rev-parse", "--absolute-git-dir"]).is_some() {
            auth_git::install(cwd)?;
        }
        return Ok(true);
    }
    if operation == "fetch" {
        return Ok(false);
    }

    // An installed checkout helper must not contaminate the ordinary probe.
    // Clone and missing-repository recovery already run from the workspace
    // root. A probe invoked inside a checkout uses that same root first.
    let probe_root: PathBuf = if owns_include(cwd) {
        let Ok((root, _)) = auth::project_context(cwd, None) else {
            return Ok(false);
        };
        if owns_include(&root) {
            return Ok(false);
        }
        root
    } else {
        cwd.to_path_buf()
    };
    let original_ssh =
        remote.starts_with("ssh://") || (!remote.contains("://") && remote.contains(':'));
    let transport = match probe(&probe_root, remote, original_ssh)? {
        Probe::Success if original_ssh && ssh_url(remote).is_some() => {
            auth::GitFallbackTransport::Ssh
        }
        Probe::Success => auth::GitFallbackTransport::Ambient,
        Probe::AuthenticationFailure if !original_ssh => {
            let Some(ssh) = ssh_url(remote) else {
                return Ok(false);
            };
            if !matches!(probe(&probe_root, &ssh, true)?, Probe::Success) {
                return Ok(false);
            }
            auth::GitFallbackTransport::Ssh
        }
        _ => return Ok(false),
    };
    auth::record_git_fallback(cwd, remote, &credential, transport)?;
    if raw_git(cwd, &["rev-parse", "--absolute-git-dir"]).is_some() {
        auth_git::install(cwd)?;
    }
    crate::human!("Git authentication: using verified ordinary Git access for Bitbucket; the saved token is unchanged.");
    Ok(true)
}
