#![cfg(unix)]
mod common;

use common::{git, unique_temp_dir};
use knit::providers::{
    bitbucket::Bitbucket, forgejo::Forgejo, github::GitHub, gitlab::GitLab, Forge, PrTarget,
};
use knit::{auth, auth_git};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

// Each scenario owns its process environment, Git config, personal store and cwd.
// No user's credential helpers, provider sessions or Git configuration are read.
fn isolated(name: &str) -> Option<PathBuf> {
    if std::env::var("AUTH_SECURITY_CHILD").as_deref() == Ok(name) {
        return Some(std::env::current_dir().unwrap());
    }
    let root = unique_temp_dir();
    for dir in ["home", "bin"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env_clear()
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                root.join("bin").display()
            ),
        )
        .env("HOME", root.join("home"))
        .env("KNIT_HOME", root.join("home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("empty.gitconfig"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("AUTH_SECURITY_CHILD", name)
        .current_dir(&root)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(root);
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    None
}
fn write_json(path: impl AsRef<Path>, value: Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}
fn workspace(root: &Path, name: &str, host: &str) -> PathBuf {
    let workspace = root.join(name);
    fs::create_dir_all(workspace.join(".knit/projects")).unwrap();
    fs::create_dir_all(workspace.join("app")).unwrap();
    git(&workspace.join("app"), ["init", "-q"]);
    git(
        &workspace.join("app"),
        [
            "remote",
            "add",
            "origin",
            &format!("https://{host}/org/app.git"),
        ],
    );
    write_json(
        workspace.join(".knit/config.json"),
        json!({"schemaVersion":"1", "activeProject":"same"}),
    );
    for id in ["same", "other"] {
        write_json(
            workspace.join(format!(".knit/projects/{id}.project.json")),
            json!({
                "schemaVersion":"1", "kind":"KnitProject", "id":id, "createdAt":"", "updatedAt":"",
                "repos":[{"id":"app", "path":"app", "remote":format!("https://{host}/org/app.git"), "baseBranch":"main"}]
            }),
        );
    }
    workspace
}
fn executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn actual_project_resolution_routes_saved_and_environment_tokens_to_git_and_cli() {
    let Some(root) =
        isolated("actual_project_resolution_routes_saved_and_environment_tokens_to_git_and_cli")
    else {
        return;
    };
    let one = workspace(&root, "one", "github.com");
    let two = workspace(&root, "two", "github.com");
    let mut projects = serde_json::Map::new();
    for (ws, id, name) in [
        (&one, "same", "saved"),
        (&one, "other", "env"),
        (&two, "same", "env"),
        (&two, "other", "saved"),
    ] {
        projects.insert(auth::project_key(ws, id).unwrap(), json!({"app":name}));
    }
    write_json(
        root.join("home/forge-auth.json"),
        json!({"credentials":{
        "saved":{"provider":"github", "host":"github.com"},
        "env":{"provider":"github", "host":"github.com", "tokenEnv":"SYNTHETIC_TOKEN"}}, "projects": projects}),
    );
    write_json(
        root.join("home/forge-secrets.json"),
        json!({"saved":"saved-secret"}),
    );
    std::env::set_var("SYNTHETIC_TOKEN", "env-secret");
    std::env::set_var("GH_TOKEN", "ambient-secret");
    executable(&root.join("bin/gh"), "#!/bin/sh\n[ \"$GH_TOKEN\" = \"$EXPECTED_TOKEN\" ] || exit 71\n[ \"$GITHUB_TOKEN\" = \"$EXPECTED_TOKEN\" ] || exit 72\n[ \"$GH_HOST\" = github.com ] || exit 73\nprintf '[]\\n'\n");
    for (ws, id, token, encoded) in [
        (
            &one,
            "same",
            "saved-secret",
            "eC1hY2Nlc3MtdG9rZW46c2F2ZWQtc2VjcmV0",
        ),
        (
            &one,
            "other",
            "env-secret",
            "eC1hY2Nlc3MtdG9rZW46ZW52LXNlY3JldA==",
        ),
        (
            &two,
            "same",
            "env-secret",
            "eC1hY2Nlc3MtdG9rZW46ZW52LXNlY3JldA==",
        ),
        (
            &two,
            "other",
            "saved-secret",
            "eC1hY2Nlc3MtdG9rZW46c2F2ZWQtc2VjcmV0",
        ),
    ] {
        auth::set_project_override(Some(id.into()));
        let repo = ws.join("app");
        assert_eq!(auth::resolve(&repo, None).unwrap().unwrap().token, token);
        let mut command = Command::new("git");
        let selected =
            auth_git::configure(&repo, &["fetch".into(), "origin".into()], &mut command).unwrap();
        assert_eq!(selected.len(), 1);
        // Real Git interprets the generated config; no network command is run.
        let result = command
            .current_dir(&repo)
            .args([
                "config",
                "--get-urlmatch",
                "http.extraHeader",
                "https://github.com/org/app.git",
            ])
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(
            String::from_utf8(result.stdout)
                .unwrap()
                .lines()
                .last()
                .unwrap(),
            format!("Authorization: Basic {encoded}")
        );
        std::env::set_var("EXPECTED_TOKEN", token);
        assert!(GitHub
            .find_existing(&PrTarget::checkout(&repo), "topic", "main")
            .unwrap()
            .is_none());
        assert!(GitHub
            .find_existing(&PrTarget::explicit(&repo, "org/app"), "topic", "main")
            .unwrap()
            .is_none());
    }
    assert_eq!(std::env::var("GH_TOKEN").unwrap(), "ambient-secret");
}

#[test]
fn all_native_providers_reject_broken_bindings_before_transport() {
    let Some(root) = isolated("all_native_providers_reject_broken_bindings_before_transport")
    else {
        return;
    };
    // A loopback listener detects attempted fallback to ambient API base URLs.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    for name in [
        "KNIT_GITHUB_API_BASE",
        "KNIT_GITLAB_API_BASE",
        "KNIT_BITBUCKET_API_BASE",
        "KNIT_FORGEJO_API_BASE",
    ] {
        std::env::set_var(name, format!("http://{}", listener.local_addr().unwrap()));
    }
    for name in [
        "GH_TOKEN",
        "GITLAB_TOKEN",
        "KNIT_BITBUCKET_ACCESS_TOKEN",
        "KNIT_FORGEJO_TOKEN",
    ] {
        std::env::set_var(name, "ambient-secret");
    }
    std::env::set_var("KNIT_GITHUB_API_TRANSPORT", "native");
    for (provider, host) in [
        ("github", "github.com"),
        ("gitlab", "gitlab.com"),
        ("bitbucket", "bitbucket.org"),
        ("forgejo", "codeberg.org"),
    ] {
        let ws = workspace(&root, provider, host);
        let key = auth::project_key(&ws, "same").unwrap();
        for failure in ["unassigned", "unknown", "missing-token", "host-mismatch"] {
            let binding = if failure == "unknown" {
                "unknown"
            } else {
                "selected"
            };
            let bindings = if failure == "unassigned" {
                json!({"other":"selected"})
            } else {
                json!({"app":binding})
            };
            write_json(
                root.join("home/forge-auth.json"),
                json!({"credentials":{
                "selected":{"provider":provider,"host":if failure == "host-mismatch" { "wrong.test" } else {host},"tokenEnv":"ABSENT_SYNTHETIC_TOKEN"}},
                "scopedCredentials":["selected"], "projects":{&key:bindings}}),
            );
            let target = PrTarget::explicit(ws.join("app"), "org/app");
            let error = match provider {
                "github" => GitHub.find_existing(&target, "topic", "main").map(|_| ()),
                "gitlab" => GitLab.check_runs(&target, "1", true).map(|_| ()),
                "bitbucket" => Bitbucket
                    .find_existing(&target, "topic", "main")
                    .map(|_| ()),
                _ => Forgejo.find_existing(&target, "topic", "main").map(|_| ()),
            }
            .unwrap_err()
            .to_string();
            let expected = match failure {
                "unassigned" => "no assigned credential",
                "unknown" => "not configured",
                "missing-token" => "requires environment variable",
                _ => "host does not match",
            };
            assert!(error.contains(expected), "{provider}/{failure}: {error}");
            assert!(!error.contains("ambient-secret"));
            assert!(
                matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
            );
        }
    }
}

#[test]
fn fetch_remote_groups_must_receive_project_auth() {
    let Some(root) = isolated("fetch_remote_groups_must_receive_project_auth") else {
        return;
    };
    let ws = workspace(&root, "group", "github.com");
    let repo = ws.join("app");
    let key = auth::project_key(&ws, "same").unwrap();
    write_json(
        root.join("home/forge-auth.json"),
        json!({"credentials":{"selected":{"provider":"github","host":"github.com","tokenEnv":"SYNTHETIC_TOKEN"}}, "projects":{key:{"app":"selected"}}}),
    );
    std::env::set_var("SYNTHETIC_TOKEN", "scoped-secret");
    git(&repo, ["config", "remotes.team", "origin"]);
    // This fake HTTPS transport speaks the real Git remote-helper protocol and
    // refuses every network connection. Git itself expands the remote group.
    executable(&root.join("bin/git-remote-https"), "#!/bin/sh\nprintf '%s\\n' \"$1\" > \"$TRANSPORT_LOG\"\nwhile IFS= read -r line; do\n case \"$line\" in\n capabilities) printf '\\n' ;;\n list*) printf '\\n' ;;\n '') exit 0 ;;\n esac\ndone\n");
    std::env::set_var("GIT_EXEC_PATH", root.join("bin"));
    std::env::set_var("TRANSPORT_LOG", root.join("transport.log"));
    let mut command = Command::new("git");
    let args = ["fetch".into(), "--multiple".into(), "team".into()];
    let selected = auth_git::configure(&repo, &args, &mut command).unwrap();
    let output = command.current_dir(&repo).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join("transport.log"))
            .unwrap()
            .trim(),
        "origin"
    );
    assert_eq!(selected.len(), 1, "Git fetched origin through remotes.team, but Knit supplied no project credential or ambient-auth isolation");

    // Multi-valued groups, a repeated member, and an explicit remote alongside
    // the group all resolve each distinct repository before Git is launched.
    git(
        &repo,
        ["remote", "add", "other", "https://gitlab.com/org/other.git"],
    );
    git(&repo, ["config", "--add", "remotes.team", "other origin"]);
    let project_path = ws.join(".knit/projects/same.project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    project["repos"].as_array_mut().unwrap().push(json!({"id":"other", "path":"other", "remote":"https://gitlab.com/org/other.git", "baseBranch":"main"}));
    write_json(&project_path, project);
    let registry_path = root.join("home/forge-auth.json");
    let mut registry: Value = serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    registry["credentials"]["second"] =
        json!({"provider":"gitlab", "host":"gitlab.com", "tokenEnv":"SECOND_TOKEN"});
    let key = auth::project_key(&ws, "same").unwrap();
    registry["projects"][&key]["other"] = json!("second");
    write_json(&registry_path, registry.clone());
    std::env::set_var("SECOND_TOKEN", "second-scoped-secret");
    for args in [
        vec!["fetch", "team"],
        vec!["fetch", "--multiple", "team", "origin"],
        vec!["remote", "update", "team"],
    ] {
        let args: Vec<_> = args.into_iter().map(Into::into).collect();
        let mut command = Command::new("git");
        let selected = auth_git::configure(&repo, &args, &mut command).unwrap();
        assert_eq!(
            selected.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["selected", "second"]
        );
    }
    // Git's singleton-group plain-fetch path selects the original operand;
    // a multi-entry group expands even when a same-named remote exists.
    git(
        &repo,
        ["remote", "add", "team", "https://github.com/org/app.git"],
    );
    git(&repo, ["config", "--replace-all", "remotes.team", "other"]);
    let mut command = Command::new("git");
    let plain = ["fetch".into(), "team".into()];
    let selected = auth_git::configure(&repo, &plain, &mut command).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name, "selected");
    assert!(command
        .current_dir(&repo)
        .args(&plain)
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(
        fs::read_to_string(root.join("transport.log"))
            .unwrap()
            .trim(),
        "team"
    );
    let selected = auth_git::configure(
        &repo,
        &["fetch".into(), "--multiple".into(), "team".into()],
        &mut Command::new("git"),
    )
    .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name, "second");
    git(&repo, ["config", "remotes.team", "origin other"]);
    let mut command = Command::new("git");
    let selected = auth_git::configure(&repo, &plain, &mut command).unwrap();
    assert_eq!(selected.len(), 2);
    assert!(command
        .current_dir(&repo)
        .args(plain)
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(
        fs::read_to_string(root.join("transport.log"))
            .unwrap()
            .trim(),
        "other"
    );

    // A default fetch has no group operand, even if a group shares origin's name.
    git(&repo, ["config", "remotes.origin", "other other"]);
    let selected = auth_git::configure(&repo, &["fetch".into()], &mut Command::new("git")).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name, "selected");

    // Project-only tokens require assignments; no host default covers the
    // missing member. Fail before the caller can launch any member.
    registry["scopedCredentials"] = json!(["selected", "second"]);
    registry["projects"][&key]
        .as_object_mut()
        .unwrap()
        .remove("other");
    write_json(&registry_path, registry);
    let error = auth_git::configure(
        &repo,
        &["fetch".into(), "--multiple".into(), "team".into()],
        &mut Command::new("git"),
    )
    .err()
    .expect("unassigned group member must stop the fetch");
    assert!(error.to_string().contains("no assigned credential"));
    git(&repo, ["config", "remotes.invalid", "not-a-remote"]);
    assert!(auth_git::configure(
        &repo,
        &["fetch".into(), "--multiple".into(), "invalid".into()],
        &mut Command::new("git")
    )
    .is_err());
}

#[test]
fn wizard_pty_rejects_invalid_selection_and_hides_saved_token() {
    let root = unique_temp_dir();
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/auth_wizard_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_knit"))
        .arg(&root)
        // The fixture must never fall back to the test runner's cwd: `knit
        // auth` commands activate the invoking cwd's project.
        .current_dir(&root)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(root);
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn failed_rotation_must_preserve_previously_saved_token() {
    use std::io::Write;
    use std::process::Stdio;
    let Some(root) = isolated("failed_rotation_must_preserve_previously_saved_token") else {
        return;
    };
    // A valid selected record alongside a damaged unrelated record is still
    // readable. Saving the registry rejects the latter. A failed replacement
    // must not silently rotate the token used by every existing assignment.
    write_json(
        root.join("home/forge-auth.json"),
        json!({"credentials":{
            "selected":{"provider":"github","host":"github.com"},
            "damaged":{"provider":"unsupported","host":"forge.test"}
        }}),
    );
    let secrets = root.join("home/forge-secrets.json");
    write_json(&secrets, json!({"selected":"original-synthetic-token"}));
    let before = fs::read(&secrets).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_knit"))
        .args([
            "auth",
            "add",
            "selected",
            "--provider",
            "github",
            "--token-stdin",
            "--replace",
        ])
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"replacement-synthetic-token\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Unsupported forge credential provider")
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("replacement-synthetic-token"));
    assert_eq!(
        fs::read(&secrets).unwrap(),
        before,
        "auth add reported failure but changed the existing token"
    );
}

#[test]
fn credential_source_switches_preserve_state_when_secret_store_cannot_be_read() {
    use std::io::Write;
    use std::process::Stdio;
    let Some(root) =
        isolated("credential_source_switches_preserve_state_when_secret_store_cannot_be_read")
    else {
        return;
    };
    let invoke = |args: &[&str], token: Option<&str>| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_knit"))
            .args(args)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(token) = token {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(token.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    };
    let saved = [
        "auth",
        "add",
        "selected",
        "--provider",
        "github",
        "--token-stdin",
        "--replace",
    ];
    let env = [
        "auth",
        "add",
        "selected",
        "--provider",
        "github",
        "--token-env",
        "SOURCE_TOKEN",
        "--replace",
    ];
    std::env::set_var("SOURCE_TOKEN", "environment-synthetic-token");
    assert!(invoke(&saved, Some("saved-synthetic-token\n"))
        .status
        .success());
    assert_eq!(
        auth::credential("selected").unwrap().token,
        "saved-synthetic-token"
    );
    assert!(invoke(&env, None).status.success());
    assert_eq!(
        auth::credential("selected").unwrap().token,
        "environment-synthetic-token"
    );
    let secrets_path = root.join("home/forge-secrets.json");
    let registry_path = root.join("home/forge-auth.json");
    assert!(!fs::read_to_string(&secrets_path)
        .unwrap()
        .contains("saved-synthetic-token"));
    let registry_before = fs::read(&registry_path).unwrap();
    let secrets_before = fs::read(&secrets_path).unwrap();
    fs::write(&secrets_path, "{").unwrap();
    let failure = invoke(&saved, Some("replacement-synthetic-token\n"));
    assert!(!failure.status.success());
    assert!(!String::from_utf8_lossy(&failure.stderr).contains("replacement-synthetic-token"));
    assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
    assert_eq!(
        auth::credential("selected").unwrap().token,
        "environment-synthetic-token"
    );
    fs::write(&secrets_path, secrets_before).unwrap();
    assert!(invoke(&saved, Some("replacement-synthetic-token\n"))
        .status
        .success());
    assert_eq!(
        auth::credential("selected").unwrap().token,
        "replacement-synthetic-token"
    );
    assert!(!fs::read_to_string(&registry_path)
        .unwrap()
        .contains("replacement-synthetic-token"));
}
