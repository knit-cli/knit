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
