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
            (
                status,
                fs::read_to_string(dir.join("forge-credentials.json"))
                    .unwrap_or_else(|_| "{\"data\":[]}".to_string()),
            )
        } else {
            (
                status,
                "{\"error\":{\"detail\":\"forge credential export forbidden\"}}".to_string(),
            )
        }
    } else if path == "/api/v1/me/access-token" {
        match fs::read_to_string(dir.join("access-token.json")) {
            Ok(body) => (200, body),
            Err(_) => (404, "{}".to_string()),
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
    let script = include_str!("fixtures/forge_git.sh")
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
        output.contains("skipped; local credentials cover every forge repository"),
        "{output}"
    );
    assert!(!output.contains("No selected credential for"), "{output}");
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
        output.contains("the saved credential `work` failed authentication"),
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

    // Failed clones retain their personal assignment for a normal pull retry.
    assert!(!target.join("backend").exists());
    let store: Value =
        serde_json::from_str(&fs::read_to_string(home.join("forge-auth.json")).unwrap()).unwrap();
    let assignments = store["projects"].as_object().unwrap();
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments.values().next().unwrap()["backend"], "work");

    // Repair the saved token and retry in the existing workspace without flags.
    let (out, err, ok) = knit_run(
        &root,
        &[
            "auth",
            "add",
            "work",
            "--provider",
            "github",
            "--token-stdin",
            "--replace",
        ],
        &env,
        Some("replacement-secret\n"),
    );
    assert!(ok, "{out}{err}");
    fs::remove_file(root.join("forge-rejects")).unwrap();
    let path = fake_path_env(&fake_bin);
    let mut retry_env = env.clone();
    retry_env.push(("PATH", &path));
    retry_env.push(("KNIT_REMOTE_HOSTED_TOKEN", "test-ledger-token"));
    let project_path = target.join(".knit/projects/demo.project.json");
    let mut project = read_json_cargo(&project_path);
    project["repos"][0]["includeByDefault"] = json!(false);
    fs::write(&project_path, project.to_string()).unwrap();
    fs::write(
        fake_dir.join("export.json"),
        export_with_repos(&[
            ("backend", "https://gitlab.com/stale/backend.git", "private"),
            ("unrelated", "https://gitlab.com/other/repo.git", "private"),
        ]),
    )
    .unwrap();
    let (out, err, ok) = knit_run(&target, &["pull", "--bundles"], &retry_env, None);
    assert!(ok, "{out}{err}");
    assert!(target.join("backend/.git").exists(), "{out}{err}");
    assert_eq!(
        read_json_cargo(&project_path)["repos"],
        project["repos"],
        "legacy recovery changed local repository configuration"
    );
    // Restore inventory for the independent clone failure cases below.
    fs::write(
        fake_dir.join("export.json"),
        export_with_repos(&[("backend", "https://github.com/org/repo.git", "private")]),
    )
    .unwrap();
    assert!(git_calls(&root)
        .iter()
        .any(|call| call_file(call, "helper-out").contains("password=replacement-secret")));

    // A second clone cannot fetch before its personal assignments are saved.
    let lock_dir = home.join(".knit/locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let lock = lock_dir.join("forge-auth.lock");
    fs::write(&lock, std::process::id().to_string()).unwrap();
    let blocked = root.join("blocked");
    let before = git_calls(&root).len();
    let (out, err, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            blocked.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--credential",
            "work",
            "--no-worktree",
        ],
        &retry_env,
        None,
    );
    fs::remove_file(lock).unwrap();
    assert!(!ok && err.contains("Another Knit process"), "{out}{err}");
    assert_eq!(
        git_calls(&root).len(),
        before,
        "Git ran before saving assignments"
    );

    // Missing local token material must not be described as a forge rejection.
    let absent_variable = format!("KNIT_TEST_ABSENT_CREDENTIAL_{}", std::process::id());
    let (out, err, ok) = knit_run(
        &root,
        &[
            "auth",
            "add",
            "work",
            "--provider",
            "github",
            "--token-env",
            &absent_variable,
            "--replace",
        ],
        &env,
        None,
    );
    assert!(ok, "{out}{err}");
    let unavailable = root.join("unavailable");
    let (out, err, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            unavailable.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--no-worktree",
        ],
        &retry_env,
        None,
    );
    assert!(!ok && err.contains("unavailable locally"), "{out}{err}");
    assert!(!err.contains("failed authentication"), "{err}");
    assert_eq!(
        git_calls(&root).len(),
        before,
        "Git ran with unavailable credentials"
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

// ---------------------------------------------------------------------------
// 4. Resuming an interrupted older clone: the target holds a legitimate git
//    checkout of an exported repository plus empty `.knit` scaffolding, but
//    no config and no project (an earlier Knit persisted its workspace only
//    after collecting repositories, and the user interrupted before that).
//    A normal clone into the SAME root must adopt the checkout — keeping its
//    branch and dirty files — and finish the workspace.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn interrupted_clone_resumes_into_same_root_adopting_matching_checkouts() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);

    let existing_source = root.join("existing-source");
    init_repo(&existing_source, "existing");
    let monty_source = root.join("monty-source");
    init_repo(&monty_source, "monty");

    let target = root.join("workspace");
    // The interrupted-alpha16 shape: adopted checkout + empty scaffolding,
    // and no `.knit/config.json` or project artifact anywhere.
    let existing = target.join("existing");
    git(
        &root,
        [
            "clone",
            "-q",
            existing_source.to_str().unwrap(),
            existing.to_str().unwrap(),
        ],
    );
    git(
        &existing,
        [
            "remote",
            "set-url",
            "origin",
            "git@github.com:org/existing.git",
        ],
    );
    // Dirty work-in-progress and a non-default branch must survive adoption.
    fs::write(existing.join("wip.txt"), "keep me").unwrap();
    git(&existing, ["checkout", "-q", "-b", "feature-wip"]);
    for dir in ["projects", "bundles", "worktrees"] {
        fs::create_dir_all(target.join(".knit").join(dir)).unwrap();
    }

    let fake_dir = root.join("fake-remote");
    let export = export_with_repos(&[
        ("existing", "https://github.com/org/existing.git", "public"),
        ("monty", "https://github.com/org/monty.git", "public"),
    ]);
    let base_url = spawn_recording_remote(&fake_dir, export);
    let fake_bin = write_fake_git(
        &root,
        &[
            (
                "https://github.com/org/existing.git",
                existing_source.to_str().unwrap(),
                "public",
            ),
            (
                "https://github.com/org/monty.git",
                monty_source.to_str().unwrap(),
                "public",
            ),
        ],
    );

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
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &[
            ("PATH", &fake_path_env(&fake_bin)),
            ("KNIT_HOME", home.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
        ],
        None,
    );
    assert!(ok, "{stdout}{stderr}");
    let output = format!("{stdout}{stderr}");
    assert!(
        output.contains("Resuming: 1 existing checkout(s)"),
        "{output}"
    );
    assert!(output.contains("using existing checkout"), "{output}");
    // Adoption preserved the dirty file and the branch...
    assert_eq!(
        fs::read_to_string(existing.join("wip.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(
        git(&existing, ["branch", "--show-current"]).trim(),
        "feature-wip"
    );
    // ...and the missing repository was cloned into the same root.
    assert!(target.join("monty/.git").exists());
    assert!(target.join(".knit/config.json").exists());
    let project = read_json_cargo(&target.join(".knit/projects/demo.project.json"));
    let remotes: Vec<&str> = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["remote"].as_str().unwrap())
        .collect();
    assert!(remotes.contains(&"https://github.com/org/existing.git"));
    assert!(remotes.contains(&"https://github.com/org/monty.git"));

    fs::remove_dir_all(root).unwrap();
}

/// Minimal JSON reader for the test's assertions (the file's shape is the
/// local project artifact contract).
fn read_json_cargo(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// 5. The resume path refuses what it cannot vouch for: unrelated files in
//    the target, and a checkout whose origin belongs to another repository.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn clone_refuses_unrelated_entries_and_foreign_checkouts() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);

    let source = root.join("source");
    init_repo(&source, "seed");
    let foreign_source = root.join("foreign-source");
    init_repo(&foreign_source, "foreign");

    let export = export_with_repos(&[("app", "https://github.com/org/app.git", "public")]);
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(&fake_dir, export.clone());
    let fake_bin = write_fake_git(
        &root,
        &[(
            "https://github.com/org/app.git",
            source.to_str().unwrap(),
            "public",
        )],
    );
    let path_env = fake_path_env(&fake_bin);
    let full_env: Vec<(&str, &str)> = vec![
        ("PATH", path_env.as_str()),
        ("KNIT_HOME", home.to_str().unwrap()),
        ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
    ];

    // A target with an unrelated file is refused before anything is written.
    let with_file = root.join("with-file");
    fs::create_dir_all(&with_file).unwrap();
    fs::write(with_file.join("notes.txt"), "mine").unwrap();
    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            with_file.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &full_env,
        None,
    );
    assert!(!ok, "{stdout}{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("unrelated entries"),
        "{stdout}{stderr}"
    );
    assert!(with_file.join("notes.txt").exists());

    // A checkout of a different repository in the target is refused too.
    let with_foreign = root.join("with-foreign");
    let foreign = with_foreign.join("app");
    git(
        &root,
        [
            "clone",
            "-q",
            foreign_source.to_str().unwrap(),
            foreign.to_str().unwrap(),
        ],
    );
    git(
        &foreign,
        [
            "remote",
            "set-url",
            "origin",
            "https://github.com/org/other.git",
        ],
    );
    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            with_foreign.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &full_env,
        None,
    );
    assert!(!ok, "{stdout}{stderr}");
    let output = format!("{stdout}{stderr}");
    assert!(
        output.contains("holds a checkout of `https://github.com/org/other.git`"),
        "{output}"
    );
    assert!(
        output.contains("repository `app` lives at `https://github.com/org/app.git`"),
        "{output}"
    );

    // An already-configured workspace points at pull recovery, not reclone.
    let configured = root.join("configured");
    fs::create_dir_all(configured.join(".knit/projects")).unwrap();
    fs::write(
        configured.join(".knit/config.json"),
        "{\"schemaVersion\":\"1\"}",
    )
    .unwrap();
    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            configured.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &full_env,
        None,
    );
    assert!(!ok, "{stdout}{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("knit pull --bundles"),
        "{stdout}{stderr}"
    );

    fs::remove_dir_all(root).unwrap();
}

// ---------------------------------------------------------------------------
// 6. A clone without any selection and without declared groups keeps ambient
//    Git access exactly as before: a public repository clones with no Knit
//    credential involved, a surrounding workspace's assignment for the same
//    target is never borrowed, and a private repository fails fast with the
//    actionable recoverable-workspace error instead of prompting or hanging.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn clone_without_selection_keeps_ambient_access_and_never_borrows_the_parent() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let env = home_env(&home, &git_config);

    // A parent workspace assigning this very target to a credential; a
    // borrowing clone would silently authenticate with its token.
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

    let pub_source = root.join("pub-source");
    init_repo(&pub_source, "pub");
    let priv_source = root.join("priv-source");
    init_repo(&priv_source, "priv");

    let export = export_with_repos(&[
        ("pub", "https://gitlab.com/acme/repo.git", "public"),
        ("secret", "https://gitlab.com/acme/secret.git", "private"),
    ]);
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(&fake_dir, export);
    let fake_bin = write_fake_git(
        &root,
        &[
            (
                "https://gitlab.com/acme/repo.git",
                pub_source.to_str().unwrap(),
                "public",
            ),
            (
                "https://gitlab.com/acme/secret.git",
                priv_source.to_str().unwrap(),
                "auth",
            ),
        ],
    );
    let target = root.join("workspace");
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
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &[
            ("PATH", &fake_path_env(&fake_bin)),
            ("KNIT_HOME", home.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
        ],
        None,
    );
    assert!(ok, "{stdout}{stderr}");
    let output = format!("{stdout}{stderr}");
    assert!(output.contains("Imported: 2 repo(s)"), "{output}");
    // A host with no saved credential keeps plain ambient Git: no Knit
    // credential helper rode the invocation, and the parent's secret stayed
    // home (the parent's own project assignment is never borrowed).
    for call in git_calls(&root) {
        let args = call_file(&call, "args");
        assert!(
            !args.contains("auth git-credential"),
            "ambient clone unexpectedly authenticated: {args}"
        );
    }
    assert!(
        walk_contains(&target, "parent-secret").is_empty(),
        "parent credential borrowed into the cloned workspace"
    );

    // The opposite is equally intentional: a personal host default is used
    // for EVERYTHING on its forge — including a public repository on the
    // host — with no per-repository binding and no flags.
    {
        let pub_gh_source = root.join("pub-gh-source");
        init_repo(&pub_gh_source, "pub-gh");
        let export_gh = export_with_repos(&[("pub", "https://github.com/org/repo.git", "public")]);
        let gh_fake_dir = root.join("fake-remote-gh");
        let gh_base = spawn_recording_remote(&gh_fake_dir, export_gh);
        let gh_fake_bin = write_fake_git(
            &root,
            &[(
                "https://github.com/org/repo.git",
                pub_gh_source.to_str().unwrap(),
                "public",
            )],
        );
        let gh_target = root.join("gh-workspace");
        let (stdout, stderr, ok) = knit_run(
            &parent,
            &[
                "clone",
                "acme/demo",
                gh_target.to_str().unwrap(),
                "--remote",
                "hosted",
                "--url",
                &gh_base,
                "--token",
                "test-token",
                "--no-worktree",
            ],
            &[
                ("PATH", &fake_path_env(&gh_fake_bin)),
                ("KNIT_HOME", home.to_str().unwrap()),
                ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
            ],
            None,
        );
        assert!(ok, "{stdout}{stderr}");
        assert!(gh_target.join("pub/.git").exists());
        let gh_calls: Vec<_> = git_calls(&root)
            .into_iter()
            .filter(|call| {
                call_file(call, "args").contains("https://github.com/org/repo.git")
                    && call_file(call, "mode") == "public"
            })
            .collect();
        assert!(
            gh_calls
                .iter()
                .any(|call| { call_file(call, "helper-out").contains("password=parent-secret") }),
            "the personal github default must authenticate even a public same-host clone"
        );
        // No per-repository binding was created for it.
        let registry = read_json_cargo(&home.join("forge-auth.json"));
        assert!(
            registry["projects"].as_object().is_none_or(|p| {
                p.values().all(|bindings| {
                    bindings
                        .get("pub")
                        .is_none_or(|v| v.as_str() != Some("other"))
                })
            }),
            "public default-covered clone must not gain repository bindings"
        );
    }

    // A private repository without any credential fails fast — no prompts, no
    // hang — and leaves the recoverable workspace behind.
    let private_target = root.join("private-workspace");
    let export_private =
        export_with_repos(&[("secret", "https://gitlab.com/acme/secret.git", "private")]);
    let private_fake_dir = root.join("fake-remote-private");
    let private_base = spawn_recording_remote(&private_fake_dir, export_private);
    let fake_bin = write_fake_git(
        &root,
        &[(
            "https://gitlab.com/acme/secret.git",
            priv_source.to_str().unwrap(),
            "auth",
        )],
    );
    let (stdout, stderr, ok) = knit_run(
        &parent,
        &[
            "clone",
            "acme/demo",
            private_target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &private_base,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &[
            ("PATH", &fake_path_env(&fake_bin)),
            ("KNIT_HOME", home.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
        ],
        None,
    );
    assert!(!ok, "{stdout}{stderr}");
    let output = format!("{stdout}{stderr}");
    assert!(
        output.contains("terminal prompts disabled"),
        "private clone must fail on the disabled prompt, not hang: {output}"
    );
    assert!(
        output.contains("recoverable workspace was created"),
        "{output}"
    );
    assert!(private_target.join(".knit/config.json").exists());

    fs::remove_dir_all(root).unwrap();
}

// ---------------------------------------------------------------------------
// 7. Resume validation is exact: a directory named after one repository may
//    not hold another repository's checkout; an ordinary folder inside a
//    parent Git repository does not masquerade as a checkout; and `.knit`
//    scaffolding reached through a symlink is refused without writing
//    anywhere outside the target.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn resume_rejects_renamed_checkouts_masqueraded_folders_and_scaffold_symlinks() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);

    let app_source = root.join("app-source");
    init_repo(&app_source, "app");
    let other_source = root.join("other-source");
    init_repo(&other_source, "other");

    let export = export_with_repos(&[
        ("app", "https://github.com/org/app.git", "public"),
        ("service", "https://github.com/org/service.git", "public"),
    ]);
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(&fake_dir, export);
    let fake_bin = write_fake_git(
        &root,
        &[(
            "https://github.com/org/app.git",
            app_source.to_str().unwrap(),
            "public",
        )],
    );
    let path_env = fake_path_env(&fake_bin);
    let full_env: Vec<(&str, &str)> = vec![
        ("PATH", path_env.as_str()),
        ("KNIT_HOME", home.to_str().unwrap()),
        ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
    ];

    // A directory named `app` holding the `service` repository's checkout:
    // the name must match its own repository, not just any export member.
    let renamed = root.join("renamed");
    let misplaced = renamed.join("app");
    git(
        &root,
        [
            "clone",
            "-q",
            other_source.to_str().unwrap(),
            misplaced.to_str().unwrap(),
        ],
    );
    git(
        &misplaced,
        [
            "remote",
            "set-url",
            "origin",
            "https://github.com/org/service.git",
        ],
    );
    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            renamed.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &full_env,
        None,
    );
    assert!(!ok, "{stdout}{stderr}");
    let output = format!("{stdout}{stderr}");
    assert!(
        output.contains("holds a checkout of `https://github.com/org/service.git`")
            && output.contains("repository `app` lives at `https://github.com/org/app.git`"),
        "{output}"
    );

    // An ordinary folder inside a parent Git repository answers Git's
    // inside-work-tree probe but is not a checkout root; adopting it would
    // clobber foreign work.
    let parent_repo = root.join("parent-repo");
    init_repo(&parent_repo, "parent");
    let inside = parent_repo.join("workspace");
    fs::create_dir_all(inside.join("app")).unwrap();
    fs::write(inside.join("app").join("draft.txt"), "not a checkout").unwrap();
    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            inside.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &full_env,
        None,
    );
    assert!(!ok, "{stdout}{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("inside another Git repository"),
        "{stdout}{stderr}"
    );
    assert!(inside.join("app/draft.txt").exists());

    // `.knit/projects` as a symlink to an external directory is refused; the
    // clone never writes through it and the external directory stays empty.
    let external = root.join("external-projects");
    fs::create_dir_all(&external).unwrap();
    let linked = root.join("linked");
    fs::create_dir_all(linked.join(".knit").join("worktrees")).unwrap();
    std::os::unix::fs::symlink(&external, linked.join(".knit").join("projects")).unwrap();
    let (stdout, stderr, ok) = knit_run(
        &root,
        &[
            "clone",
            "acme/demo",
            linked.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &full_env,
        None,
    );
    assert!(!ok, "{stdout}{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("not an interrupted clone"),
        "{stdout}{stderr}"
    );
    assert!(
        external.read_dir().unwrap().next().is_none(),
        "the clone must not write through the scaffolding symlink"
    );
    assert!(!linked.join(".knit/config.json").exists());

    fs::remove_dir_all(root).unwrap();
}

// ---------------------------------------------------------------------------
// 8. Noninteractive clone with declared groups never prompts or hangs: the
//    grouped private repository fails on the disabled raw prompt and is
//    skipped with the scriptable recovery guidance; the ungrouped public
//    repository still clones, and the workspace left behind is the
//    recoverable one pull uses.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn noninteractive_declared_group_clone_is_partial_with_scriptable_guidance() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);

    let solo_source = root.join("solo-source");
    init_repo(&solo_source, "solo");
    let pub_source = root.join("pub-source");
    init_repo(&pub_source, "pub");

    let repos = [
        ("solo", "https://github.com/org/solo.git", "private"),
        ("pub", "https://github.com/org/pub.git", "public"),
    ];
    let knit_project = serde_json::json!({
        "schemaVersion": "1", "kind": "KnitProject", "id": "demo",
        "createdAt": "", "updatedAt": "",
        "repos": [
            {"id": "solo", "path": "solo", "remote": "https://github.com/org/solo.git", "baseBranch": "main"},
            {"id": "pub", "path": "pub", "remote": "https://github.com/org/pub.git", "baseBranch": "main"}
        ],
        "auth": {"groups": [{
            "id": "gh", "name": "Work", "provider": "github",
            "host": "github.com", "repos": ["solo"], "tokenTypes": ["classic_pat"]
        }]}
    });
    let repositories: Vec<Value> = repos
        .iter()
        .map(|(id, url, visibility)| {
            json!({"localId": id, "name": id, "defaultBranch": "main",
                   "remoteUrl": url, "visibility": visibility, "metadata": {}})
        })
        .collect();
    let export = json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": knit_project,
            "repositories": repositories,
            "omittedRepositoryCount": 0,
            "bundles": [],
            "historyEvents": [],
        }
    })
    .to_string();
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(&fake_dir, export);
    let fake_bin = write_fake_git(
        &root,
        &[
            (
                "https://github.com/org/solo.git",
                solo_source.to_str().unwrap(),
                "auth",
            ),
            (
                "https://github.com/org/pub.git",
                pub_source.to_str().unwrap(),
                "public",
            ),
        ],
    );
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
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &[
            ("PATH", &fake_path_env(&fake_bin)),
            ("KNIT_HOME", home.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
        ],
        None,
    );
    // Piped stdin is not a terminal: no prompt, no hang, a partial success.
    assert!(ok, "{stdout}{stderr}");
    let output = format!("{stdout}{stderr}");
    assert!(output.contains("Auth requirements:"), "{output}");
    assert!(output.contains("1 credential group(s)"), "{output}");
    assert!(output.contains("Run `knit auth setup`"), "{output}");
    assert!(output.contains("Skipped: 1 repo(s)"), "{output}");
    assert!(target.join("pub/.git").exists());
    assert!(!target.join("solo/.git").exists());
    assert!(target.join(".knit/config.json").exists());

    fs::remove_dir_all(root).unwrap();
}

// ---------------------------------------------------------------------------
// 9. Partial explicit selection: `--credential` covers the GitHub host only;
//    the public GitLab repository clones ambiently. The explicit assignments
//    are persisted before ambient access is recorded, so the now-active
//    strict gate still lets the public repository through — proven with a
//    post-clone `knit auth status --check` in the new workspace.
// ---------------------------------------------------------------------------
#[cfg(unix)]
#[test]
fn partial_selection_records_ambient_after_assignments_so_check_passes() {
    let root = unique_temp_dir();
    let (home, git_config) = isolated_home(&root);
    let env = home_env(&home, &git_config);

    let app_source = root.join("app-source");
    init_repo(&app_source, "app");
    let docs_source = root.join("docs-source");
    init_repo(&docs_source, "docs");

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

    let export = export_with_repos(&[
        ("app", "git@github.com:org/repo.git", "private"),
        ("docs", "https://gitlab.com/acme/docs.git", "public"),
    ]);
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_recording_remote(&fake_dir, export);
    fs::write(fake_dir.join("forge-credentials-status"), "200").unwrap();
    fs::write(
        fake_dir.join("forge-credentials.json"),
        json!({"data":[
            {"connected":true,"hosts":["github.com","gitlab.com"]}
        ]})
        .to_string(),
    )
    .unwrap();
    let (out, err, ok) = knit_run(
        &root,
        &[
            "remote",
            "add",
            "hosted",
            &base_url,
            "--global",
            "--token-stdin",
        ],
        &env,
        Some("test-ledger-token\n"),
    );
    assert!(ok, "{out}{err}");
    if git_config.exists() {
        fs::remove_file(&git_config).unwrap();
    }
    let exports_before = forge_credential_requests(&fake_dir);
    let fake_bin = write_fake_git(
        &root,
        &[
            (
                "git@github.com:org/repo.git",
                app_source.to_str().unwrap(),
                "auth",
            ),
            (
                "https://gitlab.com/acme/docs.git",
                docs_source.to_str().unwrap(),
                "public",
            ),
        ],
    );
    let target = root.join("workspace");
    let path_env = fake_path_env(&fake_bin);
    let clone_env: Vec<(&str, &str)> = vec![
        ("PATH", path_env.as_str()),
        ("KNIT_HOME", home.to_str().unwrap()),
        ("GIT_CONFIG_GLOBAL", git_config.to_str().unwrap()),
    ];
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
            "--token",
            "test-token",
            "--prefer-https",
            "--credential",
            "work",
            "--no-worktree",
        ],
        &clone_env,
        None,
    );
    assert!(ok, "{stdout}{stderr}");
    assert!(target.join("app/.git").exists());
    assert!(target.join("docs/.git").exists());
    assert!(
        forge_credential_requests(&fake_dir) > exports_before,
        "ambient allowance must not suppress helper setup"
    );
    let probes: Vec<_> = git_calls(&root)
        .into_iter()
        .filter(|call| {
            call_file(call, "mode") == "auth" && call_file(call, "args").contains("ls-remote")
        })
        .collect();
    assert!(
        !probes.is_empty(),
        "prefer-HTTPS must exercise the selected repository probe"
    );
    for probe in probes {
        assert!(
            call_file(&probe, "helper-out").contains(&format!("password={SELECTED_SECRET}")),
            "prefer-HTTPS bypassed the local credential"
        );
    }
    assert!(
        fs::read_to_string(&git_config)
            .unwrap()
            .contains("gitlab.com"),
        "uncovered host helper not installed"
    );

    // The public GitLab repository must keep working under the strict gate
    // the explicit GitHub assignment activated: its exact remote was recorded
    // as ambient access after the assignments were persisted.
    let (stdout, stderr, ok) = knit_run(
        &target,
        &["auth", "status", "--project", "demo", "--check"],
        &clone_env,
        None,
    );
    assert!(ok, "{stdout}{stderr}");
    let output = format!("{stdout}{stderr}");
    assert!(
        !output.contains("no assigned credential"),
        "public repository lost ambient access after the partial selection: {output}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn ordinary_ledger_token_skips_optional_export_but_environment_denials_remain_visible() {
    for (kind, scopes, skip) in [
        ("legacy", vec!["project:read", "bundle:read"], true),
        (
            "environment_client",
            vec!["project:read", "forge:credential"],
            false,
        ),
    ] {
        for prefer_https in [false, true] {
            let root = unique_temp_dir();
            let (home, git_config) = isolated_home(&root);
            let source = root.join("source");
            init_repo(&source, "app");
            let url = "https://github.com/team/app.git";
            let fake_bin = write_fake_git(&root, &[(url, source.to_str().unwrap(), "public")]);
            let path = fake_path_env(&fake_bin);
            let mut env = home_env(&home, &git_config);
            env.push(("PATH", &path));
            let remote_dir = root.join("remote");
            let base =
                spawn_recording_remote(&remote_dir, export_with_repos(&[("app", url, "public")]));
            fs::write(
                remote_dir.join("access-token.json"),
                json!({"data":{"tokenKind":kind,"scopes":scopes}}).to_string(),
            )
            .unwrap();
            let (out, err, ok) = knit_run(
                &root,
                &[
                    "remote",
                    "add",
                    "hosted",
                    &base,
                    "--global",
                    "--token-stdin",
                ],
                &env,
                Some(
                    "synthetic-ledger-token
",
                ),
            );
            assert!(ok, "{out}{err}");
            let mut args = vec!["clone", "demo", "--remote", "hosted", "--no-worktree"];
            if prefer_https {
                args.push("--prefer-https");
            }
            let (out, err, ok) = knit_run(&root, &args, &env, None);
            assert!(ok, "{out}{err}");
            let output = format!("{out}{err}");
            assert!(root.join("demo/app/.git").exists());
            assert_eq!(
                forge_credential_requests(&remote_dir),
                usize::from(!skip) * (1 + usize::from(prefer_https))
            );
            if skip {
                assert!(
                    !output.contains("credential helper setup skipped"),
                    "{output}"
                );
                assert!(!output.contains("HTTP 403"), "{output}");
                // The explicit diagnostic action still reports the restriction.
                let (out, err, ok) =
                    knit_run(&root, &["remote", "sync-helpers", "hosted"], &env, None);
                assert!(!ok);
                assert!(format!("{out}{err}").contains("knit auth"));
            } else {
                assert!(
                    output.contains("environment helper authorization"),
                    "{output}"
                );
            }
            fs::remove_dir_all(root).unwrap();
        }
    }
}
