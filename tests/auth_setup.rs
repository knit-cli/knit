use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    root: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static ID: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "knit-auth-setup-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join(".knit/projects")).unwrap();
        let home = root.join("personal");
        fs::write(
            root.join(".knit/config.json"),
            json!({"schemaVersion":"0.1","activeProject":"tools"}).to_string(),
        )
        .unwrap();
        for project in ["tools", "work"] {
            fs::write(root.join(format!(".knit/projects/{project}.project.json")), json!({
                "schemaVersion":"0.1", "kind":"KnitProject", "id":project,
                "createdAt":"2026-01-01T00:00:00Z", "updatedAt":"2026-01-01T00:00:00Z",
                "repos":[
                    {"id":"personal", "path":root.join("personal-repo"), "remote":"https://github.com/person/repo.git", "baseBranch":"main"},
                    {"id":"org", "path":root.join("org-repo"), "remote":"git@github.com:company/repo.git", "baseBranch":"main"},
                    {"id":"bb", "path":root.join("bb-repo"), "remote":"https://bitbucket.org/company/repo.git", "baseBranch":"main"}
                ]
            }).to_string()).unwrap();
        }
        Self { root, home }
    }
    fn run(&self, args: &[&str], input: Option<&str>) -> Output {
        self.run_at(&self.root, args, input)
    }
    fn run_at(&self, cwd: &Path, args: &[&str], input: Option<&str>) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_knit"))
            .args(args)
            .current_dir(cwd)
            .env("KNIT_HOME", &self.home)
            .env("KNIT_TEST_FORGE_TOKEN", "test-secret-env")
            .env_remove("KNIT_BUNDLE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }
    fn ok(&self, args: &[&str], input: Option<&str>) -> String {
        let output = self.run(args, input);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn add(&self, name: &str) {
        self.ok(
            &[
                "auth",
                "add",
                name,
                "--provider",
                "github",
                "--token-env",
                "KNIT_TEST_FORGE_TOKEN",
            ],
            None,
        );
    }
    fn status(&self, project: &str) -> Value {
        serde_json::from_str(&self.ok(&["auth", "status", "--project", project, "--json"], None))
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn project_assignments_are_independent_and_never_export_tokens() {
    let f = Fixture::new();
    let before = fs::read(f.root.join(".knit/projects/tools.project.json")).unwrap();
    f.add("personal");
    f.add("classic");
    f.ok(
        &[
            "auth",
            "use",
            "personal",
            "--project",
            "tools",
            "--repo",
            "personal",
        ],
        None,
    );
    f.ok(
        &[
            "auth",
            "use",
            "classic",
            "--project",
            "tools",
            "--repo",
            "org",
        ],
        None,
    );
    let tools = f.status("tools");
    assert_eq!(tools["repositories"][0]["credential"], "personal");
    assert_eq!(tools["repositories"][1]["credential"], "classic");
    assert_eq!(
        tools["repositories"][2]["status"],
        "needs credential assignment"
    );
    assert_eq!(f.status("work")["explicit"], false);
    assert_eq!(
        before,
        fs::read(f.root.join(".knit/projects/tools.project.json")).unwrap()
    );
    let registry = fs::read_to_string(f.home.join("forge-auth.json")).unwrap();
    assert!(!registry.contains("test-secret-env"));
    assert!(!f.ok(&["auth", "list"], None).contains("test-secret-env"));
    let bad = f.run(&["auth", "use", "classic", "--repo", "bb"], None);
    assert!(!bad.status.success());
    assert_eq!(tools, f.status("tools"));
}

#[test]
fn token_storage_rotation_and_removal_are_deliberate() {
    let f = Fixture::new();
    let output = f.ok(
        &[
            "auth",
            "add",
            "classic",
            "--provider",
            "github",
            "--token-stdin",
        ],
        Some("first-test-secret\n"),
    );
    assert!(!output.contains("first-test-secret"));
    assert!(output.contains("not encrypted"));
    assert!(!fs::read_to_string(f.home.join("forge-auth.json"))
        .unwrap()
        .contains("first-test-secret"));
    let bad = f.run(
        &[
            "auth",
            "add",
            "classic",
            "--provider",
            "github",
            "--token-stdin",
        ],
        Some("second-test-secret\n"),
    );
    assert!(!bad.status.success());
    assert!(fs::read_to_string(f.home.join("forge-secrets.json"))
        .unwrap()
        .contains("first-test-secret"));
    f.ok(
        &[
            "auth",
            "add",
            "classic",
            "--provider",
            "github",
            "--token-stdin",
            "--replace",
        ],
        Some("second-test-secret\n"),
    );
    assert!(!fs::read_to_string(f.home.join("forge-secrets.json"))
        .unwrap()
        .contains("first-test-secret"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(f.home.join("forge-secrets.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    f.ok(&["auth", "use", "classic", "--repo", "org"], None);
    assert!(!f.run(&["auth", "remove", "classic"], None).status.success());
    f.ok(&["auth", "clear", "--repo", "org"], None);
    assert_eq!(f.status("tools")["explicit"], false);
    f.ok(&["auth", "remove", "classic"], None);
    assert!(!fs::read_to_string(f.home.join("forge-secrets.json"))
        .unwrap()
        .contains("second-test-secret"));
}

#[test]
fn setup_without_terminal_explains_the_scriptable_path() {
    let f = Fixture::new();
    let result = f.run(&["auth", "setup"], None);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--token-stdin"));
    assert!(!f.home.join("forge-auth.json").exists());
}

#[test]
fn worktree_context_and_explicit_project_select_the_right_assignments() {
    let f = Fixture::new();
    f.add("work-token");
    fs::create_dir_all(f.root.join(".knit/bundles")).unwrap();
    fs::create_dir_all(f.root.join(".knit/worktrees/feature/org")).unwrap();
    let mut bundle = knit::model::ChangeGroup::new(
        "feature".into(),
        "Feature".into(),
        "2026-01-01T00:00:00Z".into(),
    );
    bundle.project_id = Some("work".into());
    fs::write(
        f.root.join(".knit/bundles/feature.bundle.json"),
        serde_json::to_string(&bundle).unwrap(),
    )
    .unwrap();
    let result = f.run_at(
        &f.root.join(".knit/worktrees/feature/org"),
        &["auth", "use", "work-token", "--repo", "org"],
        None,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        f.status("work")["repositories"][1]["credential"],
        "work-token"
    );
    assert_eq!(f.status("tools")["explicit"], false);
    let result = f.run_at(
        &f.root.join(".knit/worktrees/feature/org"),
        &[
            "auth",
            "use",
            "work-token",
            "--project",
            "tools",
            "--repo",
            "personal",
        ],
        None,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        f.status("tools")["repositories"][0]["credential"],
        "work-token"
    );
}

#[test]
fn unsupported_forge_remote_fails_read_check_without_network() {
    let f = Fixture::new();
    let path = f.root.join(".knit/projects/tools.project.json");
    let mut project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    project["repos"] = json!([{"id":"unsupported", "path":f.root.join("repo"), "remote":"https://github.com:8443/person/repo.git", "baseBranch":"main"}]);
    fs::write(path, project.to_string()).unwrap();
    let output = f.run(&["auth", "status", "--check", "--json"], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("unsupported forge remote"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("local / no forge"));
}

#[test]
fn one_to_many_links_mixed_backends_and_atomic_reassignment() {
    let f = Fixture::new();
    f.add("shared");
    f.add("restricted");
    f.ok(
        &[
            "auth", "use", "shared", "--repo", "personal", "--repo", "org",
        ],
        None,
    );
    let before = f.status("tools");
    for invalid in ["bb", "missing"] {
        let result = f.run(
            &[
                "auth",
                "use",
                "restricted",
                "--repo",
                "personal",
                "--repo",
                invalid,
            ],
            None,
        );
        assert!(!result.status.success());
        assert_eq!(f.status("tools"), before);
    }
    f.ok(
        &[
            "auth",
            "add",
            "cloud",
            "--provider",
            "bitbucket",
            "--token-env",
            "KNIT_TEST_FORGE_TOKEN",
        ],
        None,
    );
    f.ok(&["auth", "use", "cloud", "--repo", "bb"], None);
    f.ok(&["auth", "use", "restricted", "--repo", "org"], None);
    let rows = f.status("tools")["repositories"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(rows[0]["credential"], "shared");
    assert_eq!(rows[1]["credential"], "restricted");
    assert_eq!(rows[2]["credential"], "cloud");
    assert!(rows.iter().all(|r| r["status"] == "configured (unchecked)"));
}

// ---------------------------------------------------------------------------
// Project-defined auth groups
// ---------------------------------------------------------------------------

fn write_auth_groups(fixture: &Fixture) {
    let path = fixture.root.join(".knit/projects/tools.project.json");
    let mut project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    project["auth"] = json!({
        "groups": [
            {
                "id": "gh", "name": "GitHub work", "provider": "github",
                "host": "github.com", "repos": ["personal", "org"],
                "tokenTypes": ["fine_grained_pat", "classic_pat"],
                "permissions": ["contents:read"],
                "instructions": "Create the token in the org.",
                "tokenUrl": "https://github.com/settings/personal-access-tokens/new"
            },
            {
                "id": "bb", "name": "Bitbucket Cloud", "provider": "bitbucket",
                "host": "bitbucket.org", "repos": ["bb"],
                "tokenTypes": ["access_token"]
            }
        ]
    });
    fs::write(path, serde_json::to_string(&project).unwrap()).unwrap();
}

#[test]
fn status_reports_auth_group_requirements_and_accurate_coverage() {
    let f = Fixture::new();
    write_auth_groups(&f);
    f.add("personal");
    f.ok(
        &[
            "auth", "use", "personal", "--repo", "personal", "--repo", "org",
        ],
        None,
    );

    // JSON: per-repo group attribution plus per-group coverage.
    let status = f.status("tools");
    let repositories = status["repositories"].as_array().unwrap().clone();
    assert_eq!(repositories[0]["authGroup"], json!("gh"));
    assert_eq!(repositories[1]["authGroup"], json!("gh"));
    assert_eq!(repositories[2]["authGroup"], json!("bb"));
    assert_eq!(repositories[0]["status"], json!("configured (unchecked)"));
    assert_eq!(
        repositories[2]["status"],
        json!("needs credential assignment (auth group `bb`)")
    );
    let groups = status["authGroups"].as_array().unwrap().clone();
    assert_eq!(groups[0]["id"], json!("gh"));
    assert_eq!(groups[0]["linked"], 2);
    assert_eq!(groups[0]["missing"], json!([]));
    assert_eq!(groups[1]["linked"], 0);
    assert_eq!(groups[1]["missing"], json!(["bb"]));
    assert_eq!(status["reposWithoutAuthGroup"], json!([]));

    // Text: the same coverage with an actionable next step.
    let text = f.ok(&["auth", "status", "--project", "tools"], None);
    assert!(
        text.contains("Auth group gh (github @ github.com): 2/2 repository(ies) linked"),
        "{text}"
    );
    assert!(
        text.contains(
            "Auth group bb (bitbucket @ bitbucket.org): 0/1 repository(ies) linked; missing: bb"
        ),
        "{text}"
    );
    assert!(
        text.contains("Run `knit auth setup --project tools` to link the missing repositories."),
        "{text}"
    );

    // A repository a group does not cover is reported as uncovered, never
    // silently assigned, and ambient authentication is still acceptable.
    let path = f.root.join(".knit/projects/tools.project.json");
    let mut project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    project["auth"]["groups"][0]["repos"] = json!(["personal"]);
    fs::write(path, serde_json::to_string(&project).unwrap()).unwrap();
    let status = f.status("tools");
    assert_eq!(status["reposWithoutAuthGroup"], json!(["org"]));
    // `personal` is still assigned and covered; shrinking the group's repo
    // list must not flip it to missing.
    assert_eq!(status["authGroups"][0]["missing"], json!([]));
    assert_eq!(status["authGroups"][0]["linked"], 1);

    // --check fails accurately while a group repository is unassigned.
    let checked = f.run(&["auth", "status", "--project", "tools", "--check"], None);
    assert!(!checked.status.success());
}

#[test]
fn auth_add_records_token_type_and_rejects_unsupported_kinds() {
    let f = Fixture::new();
    f.ok(
        &[
            "auth",
            "add",
            "fine",
            "--provider",
            "github",
            "--token-type",
            "fine_grained_pat",
            "--token-env",
            "KNIT_TEST_FORGE_TOKEN",
        ],
        None,
    );
    let registry: Value =
        serde_json::from_str(&fs::read_to_string(f.home.join("forge-auth.json")).unwrap()).unwrap();
    assert_eq!(
        registry["credentials"]["fine"]["tokenType"],
        json!("fine_grained_pat")
    );
    // Env-backed credentials keep an (empty) secret store on disk; snapshot
    // both private files after the successful add so the rejected attempt
    // can be proven not to touch either.
    let registry_bytes = fs::read(f.home.join("forge-auth.json")).unwrap();
    let secrets_bytes = fs::read(f.home.join("forge-secrets.json")).ok();
    if let Some(bytes) = &secrets_bytes {
        let secrets: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(secrets, json!({}), "env-backed credentials hold no secrets");
    }
    let rejected = f.run(
        &[
            "auth",
            "add",
            "wrong",
            "--provider",
            "github",
            "--token-type",
            "personal_access_token",
            "--token-env",
            "KNIT_TEST_FORGE_TOKEN",
        ],
        None,
    );
    assert!(!rejected.status.success());
    assert_eq!(
        fs::read(f.home.join("forge-auth.json")).unwrap(),
        registry_bytes,
        "the rejected add must not touch the registry"
    );
    assert_eq!(
        fs::read(f.home.join("forge-secrets.json")).ok(),
        secrets_bytes,
        "the rejected add must not touch the secret store"
    );
}

#[test]
fn grouped_setup_without_terminal_stays_scriptable_and_never_hangs() {
    let f = Fixture::new();
    write_auth_groups(&f);
    let result = f.run(&["auth", "setup"], None);
    // Piped stdin is not a terminal: fail fast with the scriptable path
    // instead of prompting (and never hang waiting for group answers).
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("--token-stdin"), "{stderr}");
    assert!(!f.home.join("forge-auth.json").exists());
}

#[test]
fn grouped_setup_pty_two_forges_hidden_tokens_and_absent_group_skip() {
    let root = std::env::temp_dir().join(format!("knit-auth-groups-pty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/auth_groups_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_knit"))
        .arg(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    let _ = fs::remove_dir_all(&root);
    assert!(result.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("PASS"), "{stdout}");
}
