//! Black-box regressions for the Bitbucket host-default authentication
//! fallback: when a saved host-default credential is rejected, ordinary Git
//! access — a native HTTPS credential helper, or the same repository over
//! SSH — must carry the operation before any repair is demanded, while
//! explicit `--credential` selections and per-repository overrides keep
//! failing closed. The Git side runs through a synthetic fake `git`
//! (`fixtures/fallback_git.sh`) that rejects the saved token, accepts only
//! the real ambient paths, and records every invocation; the remote side
//! through a recording fake sync remote. No real forge is contacted.

#![allow(dead_code)]
mod common;

use common::{init_repo, shell_quote, unique_temp_dir};
use serde_json::{json, Value};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BB_SECRET: &str = "bb-saved-default-secret";
const AMBIENT_SECRET: &str = "ambient-helper-secret";

// ---------------------------------------------------------------------------
// Shared harness: knit runner, isolated home, recording sync remote, export.
// ---------------------------------------------------------------------------

fn knit_run(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&str>,
) -> (String, String, bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("KNIT_HOME", common::isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", common::isolated_git_config_global())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .env_remove("KNIT_REMOTE_URL")
        .env_remove("KNIT_REMOTE_TOKEN")
        .env_remove("KNIT_REMOTE_HOSTED_TOKEN");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = match stdin {
        Some(input) => {
            command.stdin(Stdio::piped());
            let mut child = command.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        }
        None => command.output().unwrap(),
    };
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

fn isolated_home(root: &Path) -> (PathBuf, PathBuf) {
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();
    (home.clone(), home.join("gitconfig"))
}

fn home_env(home: &Path, git_config: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("KNIT_HOME", home.to_string_lossy().into_owned()),
        (
            "GIT_CONFIG_GLOBAL",
            git_config.to_string_lossy().into_owned(),
        ),
    ]
}

fn spawn_recording_remote(dir: &Path, export_body: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("export.json"), export_body).unwrap();
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let dir = dir.clone();
            std::thread::spawn(move || {
                let _ = handle_recording_remote(&mut stream, &dir);
            });
        }
    });
    base_url
}

fn handle_recording_remote(stream: &mut std::net::TcpStream, dir: &Path) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 0 {
        let mut sink = vec![0u8; content_length];
        reader.read_exact(&mut sink)?;
    }
    let path = target.split('?').next().unwrap_or_default().to_string();
    let (status, response) = if path == "/api/v1/me/forge-credentials" {
        (
            403,
            "{\"error\":{\"detail\":\"forge credential export forbidden\"}}".to_string(),
        )
    } else if path == "/api/v1/me/access-token" {
        (200, "{\"data\":{\"scopes\":[]}}".to_string())
    } else if method == "GET" && path.starts_with("/api/v1/projects/") && path.ends_with("/export")
    {
        match fs::read_to_string(dir.join("export.json")) {
            Ok(body) => (200, body),
            Err(_) => (
                404,
                "{\"error\":{\"detail\":\"no export staged\"}}".to_string(),
            ),
        }
    } else if path.starts_with("/api/v1/projects/") && path.ends_with("/view") {
        if method == "PUT" {
            (200, "{\"data\":{}}".to_string())
        } else {
            (200, "{\"data\":{\"views\":{}}}".to_string())
        }
    } else {
        (
            404,
            format!("{{\"error\":{{\"detail\":\"unexpected {method} {path}\"}}}}"),
        )
    };
    write!(
        stream,
        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        response.len(),
        response
    )?;
    stream.flush()
}

fn export_with_repos(repos: &[(&str, &str, &str)]) -> String {
    let repositories: Vec<Value> = repos
        .iter()
        .map(|(id, url, visibility)| {
            json!({
                "localId": id, "name": id, "defaultBranch": "main",
                "remoteUrl": url, "visibility": visibility, "metadata": {},
            })
        })
        .collect();
    json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": repositories,
            "omittedRepositoryCount": 0,
            "bundles": [],
            "historyEvents": [],
        }
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// The synthetic fallback git and the ambient native helper.
// ---------------------------------------------------------------------------

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

/// The exact proactive header Knit sends for the unclassified saved default
/// (its Git username is `x-token-auth`).
fn saved_auth_header() -> String {
    format!(
        "Authorization: Basic {}",
        base64(format!("x-token-auth:{BB_SECRET}").as_bytes())
    )
}

#[cfg(unix)]
fn write_fake_git(root: &Path, mapping: &[(&str, &str, &str)]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let git_path = String::from_utf8(
        Command::new("/bin/sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let fake_bin = root.join("bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let mut cases = String::new();
    for (url, source, mode) in mapping {
        cases.push_str(&format!(
            "  {}) mode={}; src={} ;;\n",
            shell_quote(url),
            mode,
            shell_quote(source)
        ));
    }
    let script = include_str!("fixtures/fallback_git.sh")
        .replace("__REAL_GIT__", &shell_quote(&git_path))
        .replace("__ROOT__", &shell_quote(&root.to_string_lossy()))
        .replace("__SAVED_HEADER__", &shell_quote(&saved_auth_header()))
        .replace("__SAVED_SECRET__", &shell_quote(BB_SECRET))
        .replace("__AMBIENT_SECRET__", &shell_quote(AMBIENT_SECRET))
        .replace("__CASES__", &cases);
    let script_path = fake_bin.join("git");
    fs::write(&script_path, script).unwrap();
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    fake_bin
}

/// A native (non-Knit) credential helper that answers only for the
/// Bitbucket host, the way an ordinary user's helper would.
#[cfg(unix)]
fn write_ambient_helper(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let helper = root.join("ambient-helper.sh");
    fs::write(
        &helper,
        format!(
            "#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n  *host=bitbucket.org*) printf 'username=ambient-user\\npassword={AMBIENT_SECRET}\\n' ;;\nesac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    helper
}

#[cfg(unix)]
fn enable_ambient_helper(git_config: &Path, helper: &Path) {
    fs::write(
        git_config,
        format!(
            "[credential]\n\thelper = !{}\n",
            shell_quote(&helper.to_string_lossy())
        ),
    )
    .unwrap();
}

#[cfg(unix)]
fn fake_path_env(fake_bin: &Path) -> String {
    format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[cfg(unix)]
fn git_calls(root: &Path) -> Vec<PathBuf> {
    let mut calls: Vec<(u32, PathBuf)> = fs::read_dir(root)
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let number = entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("git-call-")?
                .parse()
                .ok()?;
            Some((number, entry.path()))
        })
        .collect();
    calls.sort_by_key(|(number, _)| *number);
    calls.into_iter().map(|(_, path)| path).collect()
}

#[cfg(unix)]
fn call_file(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap_or_default()
}

#[cfg(unix)]
fn calls_with(root: &Path, marker: &str) -> Vec<PathBuf> {
    git_calls(root)
        .into_iter()
        .filter(|call| call.join(marker).exists())
        .collect()
}

/// Recursively list files under `root` whose content contains `needle`.
fn walk_contains(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                if String::from_utf8_lossy(&bytes).contains(needle) {
                    hits.push(path);
                }
            }
        }
    }
    hits
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// The two-repository project every scenario clones: a public repository on
/// one forge and a private Bitbucket repository served by the saved default.
const PROJECT_REPOS: &[(&str, &str, &str)] = &[
    ("web", "https://github.com/org/web.git", "public"),
    ("svc", "https://bitbucket.org/team/svc.git", "private"),
];

struct World {
    root: PathBuf,
    home: PathBuf,
    git_config: PathBuf,
    target: PathBuf,
    fake_dir: PathBuf,
    base_url: String,
    path_env: String,
}

impl World {
    fn env_vec(&self) -> Vec<(&str, &str)> {
        vec![
            ("PATH", &self.path_env),
            ("KNIT_HOME", self.home.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", self.git_config.to_str().unwrap()),
        ]
    }

    fn seed_saved_default(&self) {
        let (stdout, stderr, ok) = knit_run(
            &self.root,
            &[
                "auth",
                "add",
                "bb",
                "--provider",
                "bitbucket",
                "--token-stdin",
            ],
            &self.env_vec(),
            Some(&format!("{BB_SECRET}\n")),
        );
        assert!(ok, "{stdout}{stderr}");
        assert!(
            stdout.contains("It is the default credential for bitbucket.org."),
            "{stdout}"
        );
    }

    fn clone_project(&self) -> (String, String, bool) {
        knit_run(
            &self.root,
            &[
                "clone",
                "acme/demo",
                self.target.to_str().unwrap(),
                "--remote",
                "hosted",
                "--url",
                self.base_url.as_str(),
                "--token",
                "test-ledger-token",
                "--no-worktree",
            ],
            &self.env_vec(),
            None,
        )
    }

    fn pull_bundles(&self) -> (String, String, bool) {
        let mut env: Vec<(&str, &str)> = self.env_vec();
        env.push(("KNIT_REMOTE_HOSTED_TOKEN", "test-ledger-token"));
        knit_run(&self.target, &["pull", "--bundles"], &env, None)
    }
}

fn sources(root: &Path) -> (PathBuf, PathBuf) {
    let web = root.join("web-source");
    init_repo(&web, "web");
    let svc = root.join("svc-source");
    init_repo(&svc, "svc");
    (web, svc)
}

/// Assert the ambient-SSH safety invariants over every captured invocation.
#[cfg(unix)]
fn assert_no_unsafe_ssh_and_no_leaks(world: &World, output: &str) {
    for call in git_calls(&world.root) {
        let envdump = call_file(&call, "envdump");
        assert!(
            !envdump.contains("StrictHostKeyChecking=no")
                && !envdump.contains("UserKnownHostsFile=/dev/null"),
            "unsafe SSH options in {}:\n{envdump}",
            call.display()
        );
    }
    let header = saved_auth_header();
    assert!(!output.contains(BB_SECRET), "saved token leaked: {output}");
    assert!(!output.contains(&header), "saved header leaked: {output}");
    assert!(
        !output.contains(AMBIENT_SECRET),
        "ambient token leaked: {output}"
    );
    assert!(
        walk_contains(&world.target, BB_SECRET).is_empty(),
        "saved token written into the workspace"
    );
    // The saved default never rides the other forge's URLs.
    for call in git_calls(&world.root).into_iter().filter(|call| {
        let url = call_file(call, "url");
        url.starts_with("https://github.com/")
    }) {
        assert_eq!(call_file(&call, "helper"), "", "wrong-host helper");
        assert_eq!(call_file(&call, "header0"), "UNSET", "wrong-host header");
        assert!(
            !call_file(&call, "envdump").contains(BB_SECRET),
            "saved token carried to the other forge"
        );
    }
}

/// Assert the personal registry still holds exactly the one saved default:
/// no new tokens, no per-repository bindings.
#[cfg(unix)]
fn assert_registry_untouched(world: &World) {
    let registry = read_json(&world.home.join("forge-auth.json"));
    let credentials = registry["credentials"].as_object().unwrap();
    assert_eq!(credentials.len(), 1, "no new credentials: {credentials:?}");
    assert_eq!(
        registry["defaults"],
        json!({"bitbucket.org": "bb"}),
        "the saved default stands"
    );
    assert!(
        registry["projects"]
            .as_object()
            .is_none_or(|p| p.is_empty()),
        "no per-repository assignments: {}",
        registry["projects"]
    );
    assert_eq!(
        read_json(&world.home.join("forge-secrets.json")),
        json!({"bb": BB_SECRET}),
        "no new tokens saved"
    );
}

// ---------------------------------------------------------------------------
// 1. Rejected Bitbucket default + working native HTTPS helper: the full
//    project clone succeeds through the fallback with no prompt, no new
//    token, and no per-repository assignments.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn rejected_default_falls_back_to_native_https_helper_for_full_clone() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let (web_source, svc_source) = sources(&root);
    let ambient = write_ambient_helper(&root);
    enable_ambient_helper(&git_config, &ambient);
    let fake_bin = write_fake_git(
        &root,
        &[
            (
                "https://github.com/org/web.git",
                web_source.to_str().unwrap(),
                "public",
            ),
            (
                "https://bitbucket.org/team/svc.git",
                svc_source.to_str().unwrap(),
                "reject-saved",
            ),
            (
                "git@bitbucket.org:team/svc.git",
                svc_source.to_str().unwrap(),
                "ssh-refused",
            ),
        ],
    );
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(&fake_dir, export_with_repos(PROJECT_REPOS));
    let target = root.join("workspace");
    let world = World {
        path_env: fake_path_env(&fake_bin),
        root,
        home,
        git_config,
        target,
        fake_dir,
        base_url,
    };
    world.seed_saved_default();

    let (stdout, stderr, ok) = world.clone_project();
    let output = format!("{stdout}{stderr}");
    assert!(ok, "{output}");
    assert!(world.target.join("web/.git").exists(), "{output}");
    assert!(world.target.join("svc/.git").exists(), "{output}");
    assert!(
        !output.contains("Setting up project credentials"),
        "a default-covered clone must not run guided setup: {output}"
    );
    assert!(
        !output.contains("Token for bitbucket.org"),
        "no token may be requested: {output}"
    );

    // The saved default was tried and rejected, then the native helper
    // carried the same HTTPS URL — no SSH detour, no guided repair.
    assert!(
        !calls_with(&world.root, "rejected-saved").is_empty(),
        "the saved default must be attempted first: {output}"
    );
    let ambient_calls = calls_with(&world.root, "ambient-ok");
    assert!(
        ambient_calls
            .iter()
            .any(|call| call_file(call, "url").starts_with("https://bitbucket.org/")),
        "the fallback must ride ordinary HTTPS with the native helper"
    );
    assert!(calls_with(&world.root, "ssh-ok").is_empty());

    assert_registry_untouched(&world);
    assert_no_unsafe_ssh_and_no_leaks(&world, &output);
    fs::remove_dir_all(world.root).unwrap();
}

// ---------------------------------------------------------------------------
// 2. Saved default rejected, native HTTPS unavailable, SSH works: the full
//    clone succeeds with the same host/path identity over SSH.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn rejected_default_falls_back_to_same_repo_ssh_when_https_unavailable() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let (web_source, svc_source) = sources(&root);
    let fake_bin = write_fake_git(
        &root,
        &[
            (
                "https://github.com/org/web.git",
                web_source.to_str().unwrap(),
                "public",
            ),
            (
                "https://bitbucket.org/team/svc.git",
                svc_source.to_str().unwrap(),
                "reject-saved",
            ),
            (
                "git@bitbucket.org:team/svc.git",
                svc_source.to_str().unwrap(),
                "ssh-ok",
            ),
            (
                "git@bitbucket.org:team/svc",
                svc_source.to_str().unwrap(),
                "ssh-ok",
            ),
            (
                "ssh://git@bitbucket.org/team/svc.git",
                svc_source.to_str().unwrap(),
                "ssh-ok",
            ),
            (
                "ssh://git@bitbucket.org/team/svc",
                svc_source.to_str().unwrap(),
                "ssh-ok",
            ),
        ],
    );
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(&fake_dir, export_with_repos(PROJECT_REPOS));
    let target = root.join("workspace");
    let world = World {
        path_env: fake_path_env(&fake_bin),
        root,
        home,
        git_config,
        target,
        fake_dir,
        base_url,
    };
    world.seed_saved_default();

    let (stdout, stderr, ok) = world.clone_project();
    let output = format!("{stdout}{stderr}");
    assert!(ok, "{output}");
    assert!(world.target.join("web/.git").exists(), "{output}");
    assert!(world.target.join("svc/.git").exists(), "{output}");

    // The SSH fallback is the same repository: identical host and path,
    // recorded as the checkout's origin.
    let ssh_calls = calls_with(&world.root, "ssh-ok");
    assert!(!ssh_calls.is_empty(), "no SSH fallback attempt: {output}");
    let origin = common::git(&world.target.join("svc"), ["remote", "get-url", "origin"]);
    let origin = origin.trim();
    let same_identity = origin == "git@bitbucket.org:team/svc.git"
        || origin == "git@bitbucket.org:team/svc"
        || origin == "ssh://git@bitbucket.org/team/svc.git"
        || origin == "ssh://git@bitbucket.org/team/svc";
    assert!(
        same_identity,
        "origin must keep the same host/path identity: {origin}"
    );
    // No native HTTPS helper exists here, so no invocation may claim one.
    assert!(
        calls_with(&world.root, "ambient-ok").is_empty(),
        "no ambient HTTPS exists in this scenario"
    );

    assert_registry_untouched(&world);
    assert_no_unsafe_ssh_and_no_leaks(&world, &output);
    fs::remove_dir_all(world.root).unwrap();
}

// ---------------------------------------------------------------------------
// 3a. An explicit --credential selection fails closed: the rejected
//     credential is never traded for ambient Git access.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn explicit_credential_failure_never_falls_back() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let (_, svc_source) = sources(&root);
    let ambient = write_ambient_helper(&root);
    enable_ambient_helper(&git_config, &ambient);
    let fake_bin = write_fake_git(
        &root,
        &[
            (
                "https://bitbucket.org/team/svc.git",
                svc_source.to_str().unwrap(),
                "reject-saved",
            ),
            (
                "git@bitbucket.org:team/svc.git",
                svc_source.to_str().unwrap(),
                "ssh-ok",
            ),
        ],
    );
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(
        &fake_dir,
        export_with_repos(&[("svc", "https://bitbucket.org/team/svc.git", "private")]),
    );
    let target = root.join("workspace");
    let world = World {
        path_env: fake_path_env(&fake_bin),
        root,
        home,
        git_config,
        target,
        fake_dir,
        base_url,
    };
    world.seed_saved_default();

    let (stdout, stderr, ok) = knit_run(
        &world.root,
        &[
            "clone",
            "acme/demo",
            world.target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            world.base_url.as_str(),
            "--token",
            "test-ledger-token",
            "--credential",
            "bb",
            "--no-worktree",
        ],
        &world.env_vec(),
        None,
    );
    let output = format!("{stdout}{stderr}");
    assert!(!ok, "an explicit selection must fail closed: {output}");
    assert!(
        !world.target.join("svc/.git").exists(),
        "the repository must not appear: {output}"
    );
    assert!(
        !calls_with(&world.root, "rejected-saved").is_empty(),
        "the selected credential must be attempted: {output}"
    );
    assert!(
        calls_with(&world.root, "ambient-ok").is_empty()
            && calls_with(&world.root, "ssh-ok").is_empty(),
        "an explicit selection must never fall back to ambient access"
    );
    assert_no_unsafe_ssh_and_no_leaks(&world, &output);
    fs::remove_dir_all(world.root).unwrap();
}

// ---------------------------------------------------------------------------
// 3b/4 shared setup: a partial workspace whose Bitbucket checkout is
// missing because the saved default is rejected and nothing ambient works
// yet; the sibling repository is cloned and left dirty.
// ---------------------------------------------------------------------------
#[cfg(unix)]
fn partial_workspace_without_ambient() -> (World, String) {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let (web_source, svc_source) = sources(&root);
    let fake_bin = write_fake_git(
        &root,
        &[
            (
                "https://github.com/org/web.git",
                web_source.to_str().unwrap(),
                "public",
            ),
            (
                "https://bitbucket.org/team/svc.git",
                svc_source.to_str().unwrap(),
                "reject-saved",
            ),
            (
                "git@bitbucket.org:team/svc.git",
                svc_source.to_str().unwrap(),
                "ssh-refused",
            ),
        ],
    );
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(&fake_dir, export_with_repos(PROJECT_REPOS));
    let target = root.join("workspace");
    let world = World {
        path_env: fake_path_env(&fake_bin),
        root,
        home,
        git_config,
        target,
        fake_dir,
        base_url,
    };
    world.seed_saved_default();

    let (stdout, stderr, ok) = world.clone_project();
    let output = format!("{stdout}{stderr}");
    assert!(ok, "the sibling repository must clone: {output}");
    assert!(world.target.join("web/.git").exists(), "{output}");
    assert!(
        !world.target.join("svc").exists(),
        "the Bitbucket repository starts missing: {output}"
    );

    // Dirty sibling work that a recovery must never touch.
    let dirty = world.target.join("web/app.txt");
    let original = fs::read_to_string(&dirty).unwrap();
    fs::write(&dirty, format!("{original}dirty-local-change\n")).unwrap();
    (world, original)
}

// ---------------------------------------------------------------------------
// 3b. A per-repository override fails closed even when ambient access
//     works: the pull reports the failure instead of falling back.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn per_repo_override_failure_never_falls_back() {
    let (world, original) = partial_workspace_without_ambient();
    let (stdout, stderr, ok) = knit_run(
        &world.target,
        &["auth", "use", "bb", "--project", "demo", "--repo", "svc"],
        &world.env_vec(),
        None,
    );
    assert!(ok, "{stdout}{stderr}");

    // Ambient access becomes available after the failed clone.
    let ambient = write_ambient_helper(&world.root);
    enable_ambient_helper(&world.git_config, &ambient);
    let (stdout, stderr, _ok) = world.pull_bundles();
    let output = format!("{stdout}{stderr}");
    assert!(
        !world.target.join("svc/.git").exists(),
        "a per-repository override must fail closed: {output}"
    );
    assert!(
        calls_with(&world.root, "ambient-ok").is_empty(),
        "an override must never trade the rejected credential for ambient access"
    );
    assert!(
        calls_with(&world.root, "ssh-ok").is_empty(),
        "an override must never fall back to SSH"
    );

    // The sibling and its dirty work survive the failed recovery.
    let dirty = fs::read_to_string(world.target.join("web/app.txt")).unwrap();
    assert_eq!(dirty, format!("{original}dirty-local-change\n"));
    assert_no_unsafe_ssh_and_no_leaks(&world, &output);
    fs::remove_dir_all(world.root).unwrap();
}

// ---------------------------------------------------------------------------
// 4. The missing repository of an existing partial workspace recovers via
//    `knit pull --bundles` once ordinary Git works, preserving the sibling
//    checkout and its dirty work.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn missing_repo_recovers_via_pull_bundles_with_ordinary_git() {
    let (world, original) = partial_workspace_without_ambient();

    let ambient = write_ambient_helper(&world.root);
    enable_ambient_helper(&world.git_config, &ambient);
    let (stdout, stderr, ok) = world.pull_bundles();
    let output = format!("{stdout}{stderr}");
    assert!(ok, "{output}");
    assert!(
        world.target.join("svc/.git").exists(),
        "the missing repository must recover through ordinary Git: {output}"
    );
    assert!(
        calls_with(&world.root, "ambient-ok")
            .iter()
            .any(|call| call_file(call, "url").starts_with("https://bitbucket.org/")),
        "the recovery must ride the native ambient helper"
    );

    // The sibling keeps its checkout, its dirty file, and its dirty state.
    let dirty = fs::read_to_string(world.target.join("web/app.txt")).unwrap();
    assert_eq!(dirty, format!("{original}dirty-local-change\n"));
    assert!(
        !common::git(&world.target.join("web"), ["status", "--porcelain"])
            .trim()
            .is_empty(),
        "the sibling must stay dirty"
    );

    assert_registry_untouched(&world);
    assert_no_unsafe_ssh_and_no_leaks(&world, &output);
    fs::remove_dir_all(world.root).unwrap();
}

// ---------------------------------------------------------------------------
// 5. After a successful fallback, helper installation/refresh and a later
//    plain Git fetch still authenticate: usable ordinary auth survives the
//    Knit-managed checkout state.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn fallback_survives_helper_refresh_and_later_plain_fetch() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let (web_source, svc_source) = sources(&root);
    let ambient = write_ambient_helper(&root);
    enable_ambient_helper(&git_config, &ambient);
    let fake_bin = write_fake_git(
        &root,
        &[
            (
                "https://github.com/org/web.git",
                web_source.to_str().unwrap(),
                "public",
            ),
            (
                "https://bitbucket.org/team/svc.git",
                svc_source.to_str().unwrap(),
                "reject-saved",
            ),
        ],
    );
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(&fake_dir, export_with_repos(PROJECT_REPOS));
    let target = root.join("workspace");
    let world = World {
        path_env: fake_path_env(&fake_bin),
        root,
        home,
        git_config,
        target,
        fake_dir,
        base_url,
    };
    world.seed_saved_default();

    let (stdout, stderr, ok) = world.clone_project();
    let output = format!("{stdout}{stderr}");
    assert!(ok, "{output}");
    assert!(world.target.join("svc/.git").exists(), "{output}");
    let calls_before = git_calls(&world.root).len();

    // The refresh every Knit auth flow performs, then plain Git in the
    // checkout: both must leave working authentication behind.
    let (stdout, stderr, ok) = knit_run(&world.target, &["auth", "status"], &world.env_vec(), None);
    assert!(ok, "{stdout}{stderr}");

    let fetch = Command::new("git")
        .arg("fetch")
        .current_dir(world.target.join("svc"))
        .env("PATH", &world.path_env)
        .env("KNIT_HOME", &world.home)
        .env("GIT_CONFIG_GLOBAL", &world.git_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    let fetch_output = format!(
        "{}{}",
        String::from_utf8_lossy(&fetch.stdout),
        String::from_utf8_lossy(&fetch.stderr)
    );
    assert!(
        fetch.status.success(),
        "a later fetch must keep working authentication: {fetch_output}"
    );

    let fetch_calls: Vec<_> = git_calls(&world.root)[calls_before..]
        .iter()
        .filter(|call| call_file(call, "args").contains("fetch"))
        .cloned()
        .collect();
    assert!(!fetch_calls.is_empty(), "the fetch must be captured");
    for call in fetch_calls {
        assert!(
            call.join("ambient-ok").exists(),
            "the fetch used the ambient helper: {}",
            call.display()
        );
        assert_eq!(
            call_file(&call, "header0"),
            "UNSET",
            "plain Git carries no Knit-injected header"
        );
        assert!(
            !call.join("rejected-saved").exists(),
            "the refreshed state must not push the rejected token back"
        );
    }
    assert_no_unsafe_ssh_and_no_leaks(&world, &fetch_output);
    fs::remove_dir_all(world.root).unwrap();
}
