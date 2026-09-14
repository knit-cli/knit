#![cfg(unix)]
mod common;

use common::{git, init_repo, unique_temp_dir};
use knit::providers::{github::GitHub, gitlab::GitLab, Forge, PrTarget};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// Run provider calls in a separate process so credential environment and PATH
/// are isolated even when cargo runs the surrounding test suite concurrently.
#[test]
fn project_provider_credentials_are_isolated_and_fail_closed() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    let home = root.join("personal");
    let bin = root.join("bin");
    for path in [workspace.join(".knit/projects"), home.clone(), bin.clone()] {
        fs::create_dir_all(path).unwrap();
    }
    let repos = [
        ("a", "github.com"),
        ("b", "github.com"),
        ("gitlab", "gitlab.com"),
        ("enterprise", "github.private.test"),
    ];
    for (id, host) in repos {
        let checkout = workspace.join(id);
        init_repo(&checkout, id);
        git(
            &checkout,
            [
                "remote",
                "add",
                "origin",
                &format!("https://{host}/org/{id}.git"),
            ],
        );
    }
    git(
        &workspace.join("a"),
        [
            "remote",
            "add",
            "upstream",
            "https://github.com/other/upstream.git",
        ],
    );
    fs::write(
        workspace.join(".knit/config.json"),
        json!({"schemaVersion":"1", "activeProject":"work"}).to_string(),
    )
    .unwrap();
    let project = workspace.join(".knit/projects/work.project.json");
    fs::write(&project, json!({
        "schemaVersion":"1", "kind":"KnitProject", "id":"work", "createdAt":"", "updatedAt":"",
        "repos":repos.iter().map(|(id,host)| json!({"id":id, "path":id, "remote":format!("https://{host}/org/{id}.git"), "baseBranch":"main"})).collect::<Vec<_>>()
    }).to_string()).unwrap();
    fs::write(home.join("forge-auth.json"), json!({
        "credentials": {
            "a": {"provider":"github", "host":"github.com", "tokenEnv":"TEST_CREDENTIAL_A"},
            "b": {"provider":"github", "host":"github.com", "tokenEnv":"TEST_CREDENTIAL_B"},
            "gitlab": {"provider":"gitlab", "host":"gitlab.com", "tokenEnv":"TEST_CREDENTIAL_GL"},
            "enterprise": {"provider":"github", "host":"github.private.test", "tokenEnv":"TEST_CREDENTIAL_ENTERPRISE"}
        },
        "projects": {project.canonicalize().unwrap().to_str().unwrap(): {"a":"a", "b":"b", "gitlab":"gitlab", "enterprise":"enterprise"}}
    }).to_string()).unwrap();
    for (name, script) in [
        (
            "gh",
            r#"#!/bin/sh
if [ "$1" = pr ]; then
  case "$PWD" in */a) expected=github.com/org/a ;; */b) expected=github.com/org/b ;; *) exit 94 ;; esac
  case "$*" in *"--repo $expected"*) ;; *) exit 95 ;; esac
fi
printf '%s %s %s\n' "${GH_TOKEN:-$GH_ENTERPRISE_TOKEN}" "$GH_HOST" "$*" >> "$TEST_AUTH_LOG"
case "$GH_HOST:$*" in
  github.private.test:*) case "$*" in *"--hostname github.private.test"*) ;; *) exit 93 ;; esac ;;
esac
case "$*" in
  *reject*) printf 'HTTP 401: Bad credentials: %s\n' "$GH_TOKEN" >&2; exit 1 ;;
esac
printf '[]\n'
"#,
        ),
        (
            "glab",
            r#"#!/bin/sh
[ -z "$GITLAB_REPO" ] || exit 90
[ "$GLAB_API_PROTOCOL" = https ] || exit 91
[ "$GITLAB_API_HOST" = gitlab.com ] || exit 92
case "$*" in *"--repo https://gitlab.com/org/gitlab"*) ;; *) exit 96 ;; esac
printf '%s %s %s\n' "$GITLAB_TOKEN" "$GITLAB_HOST" "$*" >> "$TEST_AUTH_LOG"
printf '[]\n'
"#,
        ),
    ] {
        let path = bin.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "provider_process_fixture", "--nocapture"])
        .current_dir(&workspace)
        .env("KNIT_HOME", home)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("TEST_PROVIDER_CHILD", "1")
        .env("TEST_AUTH_LOG", root.join("calls.log"))
        .env("TEST_CREDENTIAL_A", "selected-a")
        .env("TEST_CREDENTIAL_B", "selected-b")
        .env("TEST_CREDENTIAL_GL", "selected-gl")
        .env("TEST_CREDENTIAL_ENTERPRISE", "selected-private")
        .env("GH_TOKEN", "ambient-should-not-be-used")
        .env("GITHUB_TOKEN", "ambient-should-not-be-used")
        .env("GH_REPO", "github.com/other/unrelated")
        .env("GITLAB_TOKEN", "ambient-should-not-be-used")
        .env("GITLAB_REPO", "other/unrelated")
        .env("GLAB_API_PROTOCOL", "http")
        .env("GITLAB_API_HOST", "other.invalid")
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_GITHUB_API_TRANSPORT")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let calls = fs::read_to_string(root.join("calls.log")).unwrap();
    assert_eq!(
        calls.lines().count(),
        6,
        "selected-token auth failure must not retry: {calls}"
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("selected-a github.com "))
            .count(),
        2
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("selected-b github.com "))
            .count(),
        2
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("selected-gl gitlab.com "))
            .count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("selected-private github.private.test "))
            .count(),
        1
    );
    assert!(!calls.contains("ambient-should-not-be-used"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn provider_process_fixture() {
    if std::env::var("TEST_PROVIDER_CHILD").as_deref() != Ok("1") {
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    std::thread::scope(|scope| {
        for id in ["a", "b"] {
            let target = PrTarget::checkout(cwd.join(id));
            scope.spawn(move || {
                assert!(GitHub
                    .find_existing(&target, "topic", "main")
                    .unwrap()
                    .is_none())
            });
        }
    });
    // The explicit repository must use B's assignment even in A's checkout.
    assert!(GitHub
        .find_existing(&PrTarget::explicit(cwd.join("a"), "org/b"), "topic", "main")
        .unwrap()
        .is_none());
    // Host-less artifact targets resolve the project's self-hosted remote,
    // even when the current checkout belongs to a different public-host repo.
    assert!(GitHub
        .find_existing(
            &PrTarget::explicit(cwd.join("a"), "org/enterprise"),
            "topic",
            "main"
        )
        .unwrap()
        .is_none());
    let error = GitHub
        .find_existing(&PrTarget::checkout(cwd.join("a")), "reject", "main")
        .unwrap_err()
        .to_string();
    assert!(error.contains("[REDACTED]"), "{error}");
    assert!(!error.contains("selected-a"), "{error}");
    assert!(GitHub
        .find_existing(
            &PrTarget::explicit(cwd.join("a"), "other/unassigned"),
            "topic",
            "main"
        )
        .is_err());
    assert!(GitLab
        .find_existing(&PrTarget::checkout(cwd.join("gitlab")), "topic", "main")
        .unwrap()
        .is_none());
    // A duplicate path on another host is ambiguous even when unassigned and
    // the current checkout happens to match one of the candidates.
    let project_path = cwd.join(".knit/projects/work.project.json");
    let mut project: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["repos"].as_array_mut().unwrap().push(json!({
        "id":"duplicate", "path":"duplicate", "remote":"https://github.private.test/org/a.git", "baseBranch":"main"
    }));
    fs::write(project_path, project.to_string()).unwrap();
    let error = GitHub
        .find_existing(&PrTarget::explicit(cwd.join("a"), "org/a"), "topic", "main")
        .unwrap_err()
        .to_string();
    assert!(error.contains("ambiguous"), "{error}");
    assert_eq!(
        std::env::var("GH_TOKEN").unwrap(),
        "ambient-should-not-be-used"
    );
}

#[test]
fn project_provider_selection_uses_bound_metadata() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    let home = root.join("personal");
    fs::create_dir_all(workspace.join(".knit/projects")).unwrap();
    fs::create_dir_all(&home).unwrap();
    let checkout = workspace.join("checkout");
    init_repo(&checkout, "checkout");
    git(
        &checkout,
        [
            "remote",
            "add",
            "origin",
            "https://github.com/org/checkout.git",
        ],
    );
    fs::write(
        workspace.join(".knit/config.json"),
        json!({
            "schemaVersion":"1", "activeProject":"work"
        })
        .to_string(),
    )
    .unwrap();
    let remotes = [
        ("lab", "https://git.example.test/org/lab.git"),
        ("forge", "https://source.example.test/org/forge.git"),
    ];
    let mut projects = serde_json::Map::new();
    for (id, bindings) in [
        ("work", json!({"lab":"lab", "forge":"forge"})),
        ("other", json!({"lab":"other", "forge":"forge"})),
        ("legacy", json!({})),
    ] {
        let path = workspace.join(format!(".knit/projects/{id}.project.json"));
        fs::write(
            &path,
            json!({
                "schemaVersion":"1", "kind":"KnitProject", "id":id,
                "createdAt":"", "updatedAt":"",
                "repos": remotes.iter().map(|(id, remote)| json!({
                    "id":id, "path":"checkout", "remote":remote, "baseBranch":"main"
                })).collect::<Vec<_>>()
            })
            .to_string(),
        )
        .unwrap();
        projects.insert(
            path.canonicalize().unwrap().to_str().unwrap().into(),
            bindings,
        );
    }
    fs::write(home.join("forge-auth.json"), json!({
        "credentials": {
            "lab":{"provider":"gitlab", "host":"git.example.test", "tokenEnv":"TEST_MISSING_PROVIDER_TOKEN"},
            "forge":{"provider":"forgejo", "host":"source.example.test", "tokenEnv":"TEST_MISSING_PROVIDER_TOKEN"},
            "other":{"provider":"forgejo", "host":"git.example.test", "tokenEnv":"TEST_MISSING_PROVIDER_TOKEN"}
        },
        "projects": projects
    }).to_string()).unwrap();
    // Adapter discovery must not read the secret store, either.
    fs::write(home.join("forge-secrets.json"), "invalid secret store").unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "provider_selection_process_fixture",
            "--nocapture",
        ])
        .current_dir(&checkout)
        .env("KNIT_HOME", &home)
        .env("TEST_PROVIDER_SELECTION_CHILD", "1")
        .env_remove("TEST_MISSING_PROVIDER_TOKEN")
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .output()
        .unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn provider_selection_process_fixture() {
    if std::env::var("TEST_PROVIDER_SELECTION_CHILD").as_deref() != Ok("1") {
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    let entry = |id: &str, remote: &str| -> knit::model::RepoEntry {
        serde_json::from_value(json!({
            "id":id, "path":cwd, "remote":remote, "baseBranch":"main"
        }))
        .unwrap()
    };
    let lab = entry("lab", "https://git.example.test/org/lab.git");
    let forge = entry("forge", "https://source.example.test/org/forge.git");
    // This is the same adapter selection entry point used by publish and land.
    let adapter = knit::providers::for_repo(&lab).unwrap();
    assert_eq!(adapter.id(), "gitlab");
    assert_eq!(knit::providers::for_repo(&forge).unwrap().id(), "forgejo");
    let error = adapter
        .find_existing(&PrTarget::explicit(&cwd, "org/lab"), "topic", "main")
        .unwrap_err()
        .to_string();
    assert!(error.contains("TEST_MISSING_PROVIDER_TOKEN"), "{error}");
    assert!(!error.contains("operation uses github"), "{error}");

    knit::auth::set_project_override(Some("other".into()));
    assert_eq!(knit::providers::for_repo(&lab).unwrap().id(), "forgejo");
    knit::auth::set_project_override(Some("legacy".into()));
    for (remote, expected) in [
        ("https://git.example.test/org/lab.git", "github"),
        ("https://gitlab.com/org/lab.git", "gitlab"),
        ("https://codeberg.org/org/forge.git", "forgejo"),
        ("https://bitbucket.org/org/repo.git", "bitbucket"),
        ("/local/repo", "github"),
    ] {
        assert_eq!(
            knit::providers::for_repo(&entry("legacy", remote))
                .unwrap()
                .id(),
            expected
        );
    }
    knit::auth::set_project_override(None);
    let registry_path =
        std::path::PathBuf::from(std::env::var_os("KNIT_HOME").unwrap()).join("forge-auth.json");
    let registry: serde_json::Value =
        serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    let project_key = cwd
        .parent()
        .unwrap()
        .join(".knit/projects/work.project.json")
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    for (binding, expected) in [
        (Some("forge"), "host does not match"),
        (Some("absent"), "is not configured"),
        (None, "has no assigned credential"),
    ] {
        let mut changed = registry.clone();
        if let Some(binding) = binding {
            changed["projects"][&project_key]["lab"] = json!(binding);
        } else {
            changed["projects"][&project_key]
                .as_object_mut()
                .unwrap()
                .remove("lab");
        }
        fs::write(&registry_path, changed.to_string()).unwrap();
        let error = knit::providers::for_repo(&lab)
            .err()
            .expect("invalid binding must fail closed")
            .to_string();
        assert!(error.contains(expected), "{error}");
    }
    fs::write(&registry_path, registry.to_string()).unwrap();
    assert!(knit::providers::for_repo(&entry(
        "unknown",
        "https://git.example.test/org/unassigned.git"
    ))
    .is_err());
}
