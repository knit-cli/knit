//! Focused integration regressions for `knit clone --credential NAME`:
//! authenticating a project clone with saved personal forge credentials
//! before any local project exists. The Git side is exercised through an
//! isolated fake `git` that captures the exact per-invocation configuration
//! and drives the real hidden `knit auth git-credential` helper; the remote
//! side through a recording fake sync remote. No real forge is contacted.

#![allow(dead_code)]
mod common;

use common::{
    git, init_repo, isolated_git_config_global, isolated_knit_home, shell_quote, unique_temp_dir,
};
use serde_json::{json, Value};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ---------------------------------------------------------------------------
// Knit runner: full env control (isolated per-test KNIT_HOME), optional stdin.
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
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
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

/// A per-test isolated KNIT_HOME plus its empty global Git config.
fn isolated_home(root: &Path) -> (PathBuf, PathBuf) {
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();
    let git_config = home.join("gitconfig");
    (home, git_config)
}

fn home_env<'a>(home: &'a Path, git_config: &'a Path) -> Vec<(&'a str, &'a str)> {
    vec![
        ("KNIT_HOME", home.to_str().unwrap()),
        ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
    ]
}

// ---------------------------------------------------------------------------
// Recording fake sync remote: export + views, and the hosted forge-credential
// endpoint that clone-time helper installation would read. Every request is
// appended to `<dir>/requests.txt` as `METHOD path authorization`.
// ---------------------------------------------------------------------------

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
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "authorization" => authorization = value.trim().to_string(),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("requests.txt"))?;
    writeln!(
        log,
        "{} {} {}",
        method,
        target,
        if authorization.is_empty() {
            "-"
        } else {
            &authorization
        }
    )?;

    // The hosted forge-credential export refuses ordinary tokens by default
    // (the original repro: a plain Svartal token gets 403 here, and Git used
    // to fall through to prompts with no credential at all).
    let path = target.split('?').next().unwrap_or_default().to_string();
    let (status, response) = if path == "/api/v1/me/forge-credentials" {
        let status: u16 = fs::read_to_string(dir.join("forge-credentials-status"))
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(403);
        if (200..300).contains(&status) {
            (status, "{\"data\":[]}".to_string())
        } else {
            (
                status,
                "{\"error\":{\"detail\":\"forge credential export forbidden\"}}".to_string(),
            )
        }
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
            let mut puts = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("views-puts.jsonl"))?;
            writeln!(puts, "{}", body)?;
            (200, "{\"data\":{}}".to_string())
        } else {
            match fs::read_to_string(dir.join("views.json")) {
                Ok(body) => (200, body),
                Err(_) => (200, "{\"data\":{\"views\":{}}}".to_string()),
            }
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

fn recorded_requests(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("requests.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn forge_credential_requests(dir: &Path) -> usize {
    recorded_requests(dir)
        .iter()
        .filter(|line| line.contains("/api/v1/me/forge-credentials"))
        .count()
}

fn recorded_view_puts(dir: &Path) -> Vec<Value> {
    fs::read_to_string(dir.join("views-puts.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// A slim export with the given repositories, `(localId, remoteUrl, visibility)`.
fn export_with_repos(repos: &[(&str, &str, &str)]) -> String {
    let repositories: Vec<Value> = repos
        .iter()
        .map(|(id, url, visibility)| {
            json!({
                "localId": id,
                "name": id,
                "defaultBranch": "main",
                "remoteUrl": url,
                "visibility": visibility,
                "metadata": {},
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
// Fake git: captures every network invocation (clone / ls-remote) under
// `<root>/git-call-N/`, invokes the real hidden auth helper exactly as Git
// would, and simulates the private forge request. `mapping` entries are
// `(url, local source repo, mode)` with mode `auth` (a credential is
// required) or `public` (ambient Git succeeds without one).
// ---------------------------------------------------------------------------

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
    let script = r#"#!/bin/sh
set -u
real_git=__REAL_GIT__
root=__ROOT__
op=
url=
for arg in "$@"; do
  if [ -n "$op" ] && [ -z "$url" ]; then
    case "$arg" in
      -*) ;;
      *) url="$arg" ;;
    esac
  fi
  case "$arg" in
    clone|ls-remote) op="$arg" ;;
  esac
done
if [ -z "$op" ]; then
  exec "$real_git" "$@"
fi
n=$(cat "$root/git-seq" 2>/dev/null || echo 0)
n=$((n + 1))
printf '%s\n' "$n" > "$root/git-seq"
d="$root/git-call-$n"
mkdir -p "$d"
printf '%s\n' "$@" > "$d/args"
printf '%s' "${KNIT_GIT_AUTH_HEADER_0-UNSET}" > "$d/header0"
printf '%s|%s|%s' "${GIT_TERMINAL_PROMPT-UNSET}" "${GIT_CURL_VERBOSE-UNSET}" "${GIT_TRACE_REDACT-UNSET}" > "$d/env"
helper=
scope=
for arg in "$@"; do
  case "$arg" in
    credential.https://*.helper=!*) helper=${arg#*=!}; scope=${arg%%=*} ;;
  esac
done
if [ -n "$helper" ]; then
  t=${scope#credential.https://}
  t=${t%.helper}
  h=${t%%/*}
  p=${t#*/}
  printf 'protocol=https\nhost=%s\npath=%s\n\n' "$h" "$p" | /bin/sh -c "$helper get" > "$d/helper-out" 2> "$d/helper-err" || true
else
  : > "$d/helper-undef"
fi
mode=
src=
case "$url" in
__CASES__
  *) mode=unknown ;;
esac
printf '%s' "${mode:-unknown}" > "$d/mode"
if [ "$op" = ls-remote ]; then
  if [ -f "$root/forge-rejects" ] && [ "$mode" = auth ]; then
    printf 'fatal: Authentication failed\n' >&2
    exit 128
  fi
  exit 0
fi
if [ "$mode" = unknown ]; then
  printf 'fake git: unexpected network url %s\n' "$url" >&2
  exit 1
fi
if [ "$mode" = auth ] && [ -z "$helper" ]; then
  : > "$d/no-credential"
  printf "fatal: could not read Username for '%s': terminal prompts disabled\n" "$url" >&2
  exit 128
fi
if [ -f "$root/forge-rejects" ]; then
  : > "$d/rejected"
  printf "fatal: Authentication failed for '%s/'\n" "$url" >&2
  cat "$d/helper-out" >&2
  printf '%s\n' "${KNIT_GIT_AUTH_HEADER_0-}" >&2
  exit 128
fi
if [ "$mode" = public ] && [ -n "$helper" ]; then
  : > "$d/unexpected-helper"
fi
target=
for arg in "$@"; do target="$arg"; done
"$real_git" clone -q "$src" "$target" || exit $?
exec "$real_git" -C "$target" remote set-url origin "$url"
"#
    .replace("__REAL_GIT__", &shell_quote(&git_path))
    .replace("__ROOT__", &shell_quote(&root.to_string_lossy()))
    .replace("__CASES__", &cases);
    let script_path = fake_bin.join("git");
    fs::write(&script_path, script).unwrap();
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    fake_bin
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

const SELECTED_SECRET: &str = "clone-selected-secret";

// ---------------------------------------------------------------------------
// 1. The happy path end to end: a saved personal token authenticates the
//    private repo of a scoped clone before any local project exists, the
//    hosted helper endpoint is never read, and a surrounding workspace's
//    unrelated assignment is never borrowed.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn selected_credential_clones_scoped_private_repo_without_borrowing_the_parent() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let env = home_env(&home, &git_config);

    // An unrelated parent workspace whose `app` repo sits on the very same
    // forge target (github.com/org/repo) and is assigned to a *different*
    // credential. A clone that fell back to surrounding workspace
    // assignments would authenticate with `other`'s token.
    let parent = root.join("parent");
    fs::create_dir_all(&parent).unwrap();
    let app_source = root.join("app-source");
    init_repo(&app_source, "app");
    git(
        &app_source,
        ["remote", "add", "origin", "https://github.com/org/repo.git"],
    );
    for (args, stdin) in [
        (vec!["init", "parentproj"], None),
        (
            vec![
                "project",
                "add",
                "app",
                app_source.to_str().unwrap(),
                "--base",
                "main",
            ],
            None,
        ),
        (
            vec![
                "auth",
                "add",
                "other",
                "--provider",
                "github",
                "--token-stdin",
            ],
            Some("parent-secret\n"),
        ),
        (vec!["auth", "use", "other", "--repo", "app"], None),
    ] {
        let (stdout, stderr, ok) = knit_run(&parent, &args, &env, stdin);
        assert!(ok, "parent setup {args:?} failed: {stdout}{stderr}");
    }
    // The clone's own credential is saved before any project for it exists.
    let (stdout, stderr, ok) = knit_run(
        &parent,
        &[
            "auth",
            "add",
            "work",
            "--provider",
            "github",
            "--token-stdin",
        ],
        &env,
        Some(&format!("{SELECTED_SECRET}\n")),
    );
    assert!(ok, "{stdout}{stderr}");
    assert!(stdout.contains("Saved `work`"), "{stdout}");

    let backend = root.join("backend-source");
    init_repo(&backend, "backend");

    // The HTTPS export must carry an identity rewrite that keeps the selected
    // token's transport despite an inherited SSH workaround. `frontend` lives on
    // another host and stays outside the `--repo backend` scope.
    let fake_dir = root.join("fake-remote");
    let export = export_with_repos(&[
        ("backend", "https://github.com/org/repo.git", "private"),
        (
            "frontend",
            "https://gitlab.com/acme/frontend.git",
            "private",
        ),
    ]);
    let base_url = spawn_recording_remote(&fake_dir, export);
    let fake_bin = write_fake_git(
        &root,
        &[(
            "https://github.com/org/repo.git",
            backend.to_str().unwrap(),
            "auth",
        )],
    );
    // A globally-configured remote means the hosted forge-credential endpoint
    // WOULD be read unless the selected credential covers the whole scope.
    let (stdout, stderr, ok) = knit_run(
        &parent,
        &["remote", "add", "hosted", &base_url, "--global"],
        &env,
        None,
    );
    assert!(ok, "{stdout}{stderr}");

    let target = parent.join("cloned");
    let fake_path = fake_path_env(&fake_bin);
    let clone_env = vec![
        ("KNIT_HOME", home.to_str().unwrap()),
        ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
        ("KNIT_REMOTE_HOSTED_TOKEN", "svartal-api-token"),
        ("PATH", fake_path.as_str()),
    ];
    let (stdout, stderr, ok) = knit_run(
        &parent,
        &[
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--credential",
            "work",
            "--repo",
            "backend",
            "--no-worktree",
            "--json",
        ],
        &clone_env,
        None,
    );
    assert!(ok, "selected-credential clone failed: {stdout}{stderr}");
    let document: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout must be pure JSON ({error}): {stdout}"));
    assert_eq!(
        document["repos"],
        json!([{"id": "backend", "status": "cloned"}])
    );
    assert_eq!(document["clonedRepoCount"], 1);
    assert_eq!(document["scopeView"], json!("scope"));
    assert_eq!(document["reposOutOfScope"], json!(["frontend"]));
    assert!(target.join("backend/app.txt").exists());
    assert!(!target.join("frontend").exists());

    // The hosted helper endpoint (403 for this token — the original repro)
    // was never asked: the selection covers every scoped repository.
    let requests = recorded_requests(&fake_dir);
    assert!(
        requests
            .iter()
            .all(|line| !line.contains("/api/v1/me/forge-credentials")),
        "hosted helper endpoint contacted: {requests:?}"
    );
    // The remote API saw exactly the Svartal bearer token — never a personal
    // forge token.
    for line in &requests {
        let authorization = line.splitn(3, ' ').nth(2).unwrap();
        assert_eq!(authorization, "Bearer svartal-api-token", "{line}");
    }
    assert!(requests.iter().any(|line| line.contains("/export")));

    // The one Git invocation: exact captured configuration.
    let calls = git_calls(&root);
    let call = calls
        .iter()
        .find(|dir| call_file(dir, "mode") == "auth")
        .expect("an authenticated clone invocation");
    let args = call_file(call, "args");
    let lines: Vec<&str> = args.lines().collect();
    assert!(lines.contains(&"credential.helper="), "{args}");
    assert!(lines.contains(&"credential.useHttpPath=true"), "{args}");
    assert!(lines.contains(&"core.askPass="), "{args}");
    assert!(
        lines.contains(&"credential.https://github.com/org/repo.git.helper="),
        "scoped helper reset missing: {args}"
    );
    let helper = lines
        .iter()
        .find(|line| line.starts_with("credential.https://github.com/org/repo.git.helper=!"))
        .expect("scoped helper entry");
    assert!(
        helper.contains(
            " auth git-credential --credential 'work' --host 'github.com' --path 'org/repo.git'"
        ),
        "{args}"
    );
    assert!(
        !helper.contains("--credential other"),
        "parent workspace assignment leaked into the clone: {args}"
    );
    // The HTTPS export gets an exact identity rewrite.
    assert!(
        lines.contains(
            &"url.https://github.com/org/repo.git.insteadOf=https://github.com/org/repo.git"
        ),
        "insteadOf identity rewrite missing: {args}"
    );
    assert!(
        lines.contains(
            &"--config-env=http.https://github.com/org/repo.git.extraHeader=KNIT_GIT_AUTH_HEADER_0"
        ),
        "{args}"
    );
    // The token rode the environment and the helper, never the command line.
    let encoded = base64(format!("x-access-token:{SELECTED_SECRET}").as_bytes());
    assert!(!args.contains(SELECTED_SECRET), "{args}");
    assert!(!args.contains(&encoded), "{args}");
    assert_eq!(call_file(call, "env"), "0|UNSET|1");
    assert_eq!(
        call_file(call, "header0"),
        format!("Authorization: Basic {encoded}")
    );
    // The hidden helper is the real `knit auth git-credential`: it vended the
    // selected credential for exactly this repository.
    let helper_out = call_file(call, "helper-out");
    assert!(
        helper_out.contains("username=x-access-token"),
        "{helper_out}{}",
        call_file(call, "helper-err")
    );
    assert!(
        helper_out.contains(&format!("password={SELECTED_SECRET}")),
        "{helper_out}"
    );
    assert!(!call.join("no-credential").exists());
    assert!(!call.join("unexpected-helper").exists());

    // Knit's own output stays secret-free and explains the coverage.
    let output = format!("{stdout}{stderr}");
    assert!(!output.contains(SELECTED_SECRET), "{output}");
    assert!(
        output.contains("Credential: work (github.com) authenticates 1 repo(s)"),
        "{output}"
    );
    assert!(
        output.contains("skipped; the selected credential(s) cover every forge repository"),
        "{output}"
    );
    assert!(!output.contains("No selected credential for"), "{output}");
    assert!(output.contains("Assigned credential:"), "{output}");
    let leaked = walk_contains(&target, SELECTED_SECRET);
    assert!(leaked.is_empty(), "credential leaked into: {leaked:?}");

    // The scope view was pushed next to the user's (empty) remote views.
    let puts = recorded_view_puts(&fake_dir);
    assert_eq!(puts.len(), 1, "exactly one views upload: {puts:?}");
    assert_eq!(puts[0]["views"]["scope"]["base"], json!("none"));
    assert_eq!(puts[0]["views"]["scope"]["include"], json!(["backend"]));

    // Personal assignments retained: the parent's `app`→`other` binding is
    // untouched, and the new workspace bound `backend`→`work`.
    let store: Value =
        serde_json::from_str(&fs::read_to_string(home.join("forge-auth.json")).unwrap()).unwrap();
    let projects = store["projects"].as_object().unwrap();
    assert_eq!(projects.len(), 2, "{projects:?}");
    assert!(
        projects
            .values()
            .any(|bindings| bindings["app"] == json!("other")),
        "parent assignment changed: {projects:?}"
    );
    assert!(
        projects
            .values()
            .any(|bindings| bindings["backend"] == json!("work")),
        "clone assignment missing: {projects:?}"
    );

    // A fresh Knit process must resolve the retained assignment without the
    // invocation-only clone guard or another --credential argument.
    let (_, stderr, ok) = knit_run(&target, &["auth", "status", "--check"], &clone_env, None);
    assert!(ok, "saved credential check failed: {stderr}");
    let calls_after = git_calls(&root);
    assert!(calls_after.len() > calls.len());
    assert!(call_file(calls_after.last().unwrap(), "helper-out")
        .contains(&format!("password={SELECTED_SECRET}")));

    fs::remove_dir_all(root).unwrap();
}

// ---------------------------------------------------------------------------
// 2. A selected token the forge rejects fails closed: the failure names the
//    selected credential with a runnable rotation command, never falls back
//    to a password prompt or ambient credentials, and never prints the token.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn denied_selected_credential_fails_closed_with_an_actionable_redacted_error() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let env = home_env(&home, &git_config);

    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "auth",
            "add",
            "work",
            "--provider",
            "github",
            "--token-stdin",
        ],
        &env,
        Some(&format!("{SELECTED_SECRET}\n")),
    );
    assert!(ok, "{stdout}{stderr}");

    let backend = root.join("backend-source");
    init_repo(&backend, "backend");
    let fake_dir = root.join("fake-remote");
    let export = export_with_repos(&[("backend", "https://github.com/org/repo.git", "private")]);
    let base_url = spawn_recording_remote(&fake_dir, export);
    let fake_bin = write_fake_git(
        &root,
        &[(
            "https://github.com/org/repo.git",
            backend.to_str().unwrap(),
            "auth",
        )],
    );
    // The forge answers the authenticated request with a rejection.
    fs::write(root.join("forge-rejects"), "").unwrap();

    let target = root.join("workspace");
    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--credential",
            "work",
            "--no-worktree",
        ],
        &[
            ("KNIT_HOME", home.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
            ("PATH", fake_path_env(&fake_bin).as_str()),
        ],
        None,
    );
    assert!(!ok, "a rejected credential must fail the clone: {stdout}");
    let output = format!("{stdout}{stderr}");
    assert!(
        output.contains("the selected credential `work` was used and access was denied"),
        "{output}"
    );
    assert!(
        output.contains(
            "update it with `knit auth add work --provider github --host github.com --replace`"
        ),
        "{output}"
    );
    assert!(!output.contains(SELECTED_SECRET), "{output}");
    assert!(
        !output.contains(&base64(
            format!("x-access-token:{SELECTED_SECRET}").as_bytes()
        )),
        "{output}"
    );
    // No fallback to interactive prompts: the failure is the forge's denial.
    assert!(!output.contains("could not read Username"), "{output}");

    // The Git invocation used the selected credential (the hidden helper
    // vended it) with prompts disabled and ambient helpers reset, then the
    // forge rejected it — no second, unauthenticated attempt followed.
    let calls = git_calls(&root);
    let call = calls
        .iter()
        .find(|dir| call_file(dir, "mode") == "auth")
        .expect("an authenticated clone invocation");
    assert_eq!(calls.len(), 1, "no fallback Git attempts: {calls:?}");
    assert!(call.join("rejected").exists());
    assert!(!call.join("no-credential").exists());
    assert!(call_file(call, "args").contains("credential.helper=\n"));
    assert_eq!(call_file(call, "env"), "0|UNSET|1");
    assert!(call_file(call, "helper-out").contains(&format!("password={SELECTED_SECRET}")));

    // Nothing was cloned and no assignment was recorded for the failure.
    assert!(!target.join("backend").exists());
    let store: Value =
        serde_json::from_str(&fs::read_to_string(home.join("forge-auth.json")).unwrap()).unwrap();
    assert!(
        store["projects"]
            .as_object()
            .map(|projects| projects.is_empty())
            .unwrap_or(true),
        "{}",
        store["projects"]
    );

    fs::remove_dir_all(root).unwrap();
}

// ---------------------------------------------------------------------------
// 3. Selection validation happens before the export is fetched or the target
//    directory is touched: unknown names and two names for one host fail
//    without a single HTTP request.
// ---------------------------------------------------------------------------
#[test]
fn credential_selection_fails_before_export_fetch_or_target_mutation() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let env = home_env(&home, &git_config);

    for name in ["work", "personal"] {
        let (stdout, stderr, ok) = knit_run(
            &root,
            &["auth", "add", name, "--provider", "github", "--token-stdin"],
            &env,
            Some(&format!("{SELECTED_SECRET}-{name}\n")),
        );
        assert!(ok, "{stdout}{stderr}");
    }

    let fake_dir = root.join("fake-remote");
    let export = export_with_repos(&[("backend", "https://github.com/org/repo.git", "private")]);
    let base_url = spawn_recording_remote(&fake_dir, export);

    // Unknown name: points at the saved credentials instead.
    let unknown_target = root.join("unknown");
    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            unknown_target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--credential",
            "nope",
            "--no-worktree",
        ],
        &env,
        None,
    );
    assert!(!ok, "{stdout}{stderr}");
    let output = format!("{stdout}{stderr}");
    assert!(
        output.contains("Credential `nope` is not configured"),
        "{output}"
    );
    assert!(output.contains("run `knit auth add`"), "{output}");
    assert!(
        output.contains("Saved credentials: personal, work."),
        "{output}"
    );
    assert!(!unknown_target.exists());

    // Two names for one host are ambiguous: refuse rather than guess.
    let duplicate_target = root.join("duplicate");
    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            duplicate_target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--credential",
            "work",
            "--credential",
            "personal",
            "--no-worktree",
        ],
        &env,
        None,
    );
    assert!(!ok, "{stdout}{stderr}");
    let output = format!("{stdout}{stderr}");
    assert!(
        output.contains("`work` and `personal` are both for github.com"),
        "{output}"
    );
    assert!(output.contains("only one credential per host"), "{output}");
    assert!(!duplicate_target.exists());

    // Neither attempt reached the remote at all.
    assert!(
        !fake_dir.join("requests.txt").exists(),
        "validation failures must not contact the remote: {:?}",
        recorded_requests(&fake_dir)
    );

    fs::remove_dir_all(root).unwrap();
}
