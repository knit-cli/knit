mod common;
use common::*;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn git<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    common::git(cwd, args).trim().to_owned()
}

const UPSTREAM: &str = "https://github.com/upstream-org/library.git";
const FORK: &str = "https://github.com/contributor/library.git";

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    local: PathBuf,
    upstream: PathBuf,
    bin: PathBuf,
    api: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let (upstream, local, _) = init_remote_repo(&root, "library");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        knit(&workspace, ["init", "demo"]);
        knit(
            &workspace,
            ["project", "add", "library", local.to_str().unwrap()],
        );
        git(&local, ["remote", "set-url", "origin", UPSTREAM]);
        let bin = root.join("bin");
        let api = root.join("api");
        fs::create_dir_all(&api).unwrap();
        fs::create_dir_all(&bin).unwrap();
        write_fake_python_cli(&bin, "gh", include_str!("fixtures/fork_create_gh.py"));
        let f = Self {
            root,
            workspace,
            local,
            upstream,
            bin,
            api,
        };
        f.settings(json!({}));
        f
    }
    fn settings(&self, value: Value) {
        fs::write(
            self.api.join("settings.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }
    fn run(&self, args: &[&str]) -> String {
        knit_with_fake_gh_env(
            &self.workspace,
            args,
            &self.bin,
            &self.api,
            &[
                ("KNIT_GITHUB_API_TRANSPORT", "gh"),
                ("KNIT_FORK_POLL_INTERVAL_MS", "1"),
            ],
        )
    }
    fn fail(&self, args: &[&str]) -> String {
        knit_fails_with_fake_gh_env(
            &self.workspace,
            args,
            &self.bin,
            &self.api,
            &[
                ("KNIT_GITHUB_API_TRANSPORT", "gh"),
                ("KNIT_FORK_POLL_INTERVAL_MS", "1"),
            ],
        )
    }
    fn split(&self, args: &[&str], extra: &[(&str, &str)]) -> (String, String, bool) {
        let original = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone()).chain(std::env::split_paths(&original)),
        )
        .unwrap();
        let path = path.to_str().unwrap();
        let api = self.api.to_str().unwrap();
        let mut env = vec![
            ("PATH", path),
            ("GH_FAKE_DIR", api),
            ("KNIT_GITHUB_API_TRANSPORT", "gh"),
            ("KNIT_FORK_POLL_INTERVAL_MS", "1"),
            ("NO_COLOR", "1"),
        ];
        env.extend_from_slice(extra);
        let value = |key| {
            env.iter()
                .rev()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| *value)
        };
        if value("KNIT_GITHUB_API_TRANSPORT") == Some("native") {
            assert!(
                is_loopback_api(value("KNIT_GITHUB_API_BASE").unwrap_or("")),
                "native fixture must provide a valid loopback API URL before invoking Knit"
            );
            let home =
                value("KNIT_HOME").expect("native fixture requires an explicit isolated home");
            assert!(
                !Path::new(home).join("forge-auth.json").exists(),
                "saved native credentials ignore API overrides"
            );
        }
        knit_split_output(&self.workspace, args, &env)
    }
    fn fresh_workspace() -> Self {
        let mut f = Self::new();
        f.workspace = f.root.join("fresh");
        fs::create_dir_all(&f.workspace).unwrap();
        f
    }
    fn clone_split(&self, args: &[&str]) -> (String, String, bool) {
        let key = format!("url.file://{}/.insteadOf", self.root.display());
        self.split(
            args,
            &[
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", &key),
                ("GIT_CONFIG_VALUE_0", "https://github.com/upstream-org/"),
            ],
        )
    }
    fn post_count(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c["method"] == "POST")
            .count()
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.api.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    fn project_bytes(&self) -> Vec<u8> {
        fs::read(self.workspace.join(".knit/projects/demo.project.json")).unwrap()
    }
    fn config(&self) -> Vec<u8> {
        fs::read(self.local.join(".git/config")).unwrap()
    }
    fn pushurl(&self) -> String {
        git(
            &self.local,
            ["config", "--get-all", "remote.origin.pushurl"],
        )
    }
    fn no_posts(&self) {
        assert!(
            self.calls().iter().all(|c| c["method"] != "POST"),
            "{:?}",
            self.calls()
        );
    }
}

#[test]
fn creates_then_polls_without_changing_upstream() {
    let f = Fixture::new();
    let project: Value = serde_json::from_slice(&f.project_bytes()).unwrap();
    f.settings(json!({"delay_once": true}));
    let output = f.run(&["fork", "library", "--json"]);
    let report: Value = serde_json::from_str(&output).expect("fork --json is machine readable");
    assert_eq!(report["repos"][0]["status"], "forked");
    assert_eq!(report["repos"][0]["pushUrl"], FORK);
    assert_eq!(f.pushurl(), FORK);
    assert_eq!(
        git(&f.local, ["config", "--get", "remote.origin.url"]),
        UPSTREAM
    );
    let updated: Value = serde_json::from_slice(&f.project_bytes()).unwrap();
    assert_eq!(updated, project);
    let calls = f.calls();
    assert!(calls.iter().any(|c| c["endpoint"] == "user"));
    let post = calls.iter().position(|c| c["method"] == "POST").unwrap();
    assert_eq!(calls[post]["endpoint"], "repos/upstream-org/library/forks");
    assert!(
        calls[post + 1..]
            .iter()
            .filter(|c| c["method"] == "GET" && c["endpoint"] == "repos/contributor/library")
            .count()
            >= 2
    );
}

#[test]
fn adopts_renamed_direct_fork_with_idempotent_post() {
    let f = Fixture::new();
    f.settings(json!({"existing": true, "name": "renamed-library"}));
    let output = f.run(&["fork", "library"]);
    assert_eq!(
        output.trim(),
        "library: upstream-org/library → contributor/renamed-library (forked), pushes go to the fork"
    );
    assert_eq!(
        f.pushurl(),
        "https://github.com/contributor/renamed-library.git"
    );
    assert_eq!(f.post_count(), 1);
}

#[test]
fn indirect_fork_rejected_without_git_or_project_writes() {
    let f = Fixture::new();
    f.settings(json!({"existing": true, "indirect": true}));
    let before = (f.config(), f.project_bytes());
    f.fail(&["fork", "library"]);
    assert_eq!((f.config(), f.project_bytes()), before);
}

#[test]
fn ssh_origin_selects_ssh_fork_transport() {
    let f = Fixture::new();
    git(
        &f.local,
        [
            "remote",
            "set-url",
            "origin",
            "git@github.com:upstream-org/library.git",
        ],
    );
    f.run(&["fork"]);
    assert_eq!(f.pushurl(), "git@github.com:contributor/library.git");
}

#[test]
fn equal_pushurl_is_noop_and_different_requires_force() {
    let f = Fixture::new();
    f.settings(json!({"existing": true}));
    git(&f.local, ["config", "remote.origin.pushurl", FORK]);
    let before = f.config();
    f.run(&["fork", "library"]);
    assert_eq!(f.config(), before);
    assert_eq!(f.post_count(), 0);
    fs::write(f.api.join("calls"), "").unwrap();
    git(
        &f.local,
        [
            "config",
            "remote.origin.pushurl",
            "https://github.com/other/library.git",
        ],
    );
    let before = (f.config(), f.project_bytes());
    f.fail(&["fork", "library"]);
    assert_eq!((f.config(), f.project_bytes()), before);
    f.no_posts();
    f.run(&["fork", "library", "--force"]);
    assert_eq!(f.pushurl(), FORK);
    f.no_posts();
}

#[test]
fn multiple_pushurls_are_refused_even_with_force() {
    let f = Fixture::new();
    for url in [FORK, "https://github.com/other/library.git"] {
        git(&f.local, ["config", "--add", "remote.origin.pushurl", url]);
    }
    let before = (f.config(), f.project_bytes());
    f.fail(&["fork", "library", "--force"]);
    assert_eq!((f.config(), f.project_bytes()), before);
    f.no_posts();
}

#[test]
fn dry_run_reports_json_without_post_or_local_writes() {
    let f = Fixture::new();
    let before = (f.config(), f.project_bytes());
    let output = f.run(&["fork", "library", "--dry-run", "--json"]);
    let report: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(report["dryRun"], true);
    assert_eq!(report["repos"][0]["status"], "would create");
    assert_eq!((f.config(), f.project_bytes()), before);
    f.no_posts();
    f.settings(json!({"existing": true}));
    let (stdout, stderr, ok) = f.split(&["fork", "library", "--dry-run", "--json"], &[]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.is_empty(),
        "dry run must not announce creation: {stderr}"
    );
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["repos"][0]["status"], "exists");
    assert_eq!((f.config(), f.project_bytes()), before);
    f.no_posts();
}

#[test]
fn organization_owner_is_sent_in_create_payload() {
    let f = Fixture::new();
    f.settings(json!({"owner": "contributor-org"}));
    f.run(&["fork", "library", "--owner", "contributor-org"]);
    assert_eq!(
        f.pushurl(),
        "https://github.com/contributor-org/library.git"
    );
    let calls = f.calls();
    let post = calls.iter().find(|c| c["method"] == "POST").unwrap();
    assert_eq!(post["payload"]["organization"], "contributor-org");
}

#[test]
fn unsupported_repo_fails_but_remaining_repo_succeeds() {
    let f = Fixture::new();
    let other = f.root.join("other");
    init_repo(&other, "other");
    git(
        &other,
        [
            "remote",
            "add",
            "origin",
            "https://gitlab.com/upstream-org/other.git",
        ],
    );
    knit(
        &f.workspace,
        ["project", "add", "other", other.to_str().unwrap()],
    );
    let before = fs::read(other.join(".git/config")).unwrap();
    let (stdout, stderr, ok) = f.split(&["fork", "other", "library", "--json"], &[]);
    assert!(!ok, "{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["repos"][0]["status"], "error");
    assert!(report["repos"][0]["error"]
        .as_str()
        .unwrap()
        .contains("Knit can only create forks on github.com"));
    assert_eq!(report["repos"][1]["status"], "forked");
    assert_eq!(f.pushurl(), FORK);
    assert_eq!(fs::read(other.join(".git/config")).unwrap(), before);
}

#[test]
fn linked_worktree_observes_shared_pushurl() {
    let f = Fixture::new();
    let linked = f.root.join("linked");
    git(
        &f.local,
        ["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
    );
    f.run(&["fork", "library"]);
    assert_eq!(
        git(&linked, ["config", "--get", "remote.origin.pushurl"]),
        FORK
    );
    assert_eq!(
        git(&linked, ["config", "--get", "remote.origin.url"]),
        UPSTREAM
    );
}

fn registered_path(f: &Fixture, id: &str) -> PathBuf {
    let project: Value = serde_json::from_slice(&f.project_bytes()).unwrap();
    let repo = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|repo| repo["id"] == id)
        .expect("clone remains registered");
    let path = PathBuf::from(repo["path"].as_str().unwrap());
    if path.is_absolute() {
        path
    } else {
        f.workspace.join(path)
    }
}

#[test]
fn project_add_file_url_clones_and_registers_without_github() {
    let f = Fixture::new();
    let url = format!("file://{}", f.upstream.display());
    f.run(&["project", "add", "cloned", &url, "--no-fork"]);
    let cloned = registered_path(&f, "cloned");
    assert_eq!(
        git(&cloned, ["rev-parse", "HEAD"]),
        git(&f.local, ["rev-parse", "HEAD"])
    );
    assert_eq!(git(&cloned, ["config", "--get", "remote.origin.url"]), url);
    assert!(f.calls().is_empty());
}

#[test]
fn project_add_fork_failure_preserves_successful_local_clone() {
    let f = Fixture::new();
    let url = format!("file://{}", f.upstream.display());
    let error = f.fail(&["project", "add", "cloned", &url, "--fork"]);
    assert!(
        error.contains("Knit can only create forks on github.com"),
        "{error}"
    );
    let cloned = registered_path(&f, "cloned");
    assert_eq!(
        git(&cloned, ["rev-parse", "HEAD"]),
        git(&f.local, ["rev-parse", "HEAD"])
    );
    assert_eq!(git(&cloned, ["config", "--get", "remote.origin.url"]), url);
    f.no_posts();
}

#[test]
fn project_add_github_url_wires_fork_with_requested_owner() {
    let f = Fixture::new();
    f.settings(json!({"owner": "contributor-org"}));
    // Rewrite clone transport locally; fork discovery must use raw origin.
    let key = format!("url.file://{}.insteadOf", f.upstream.display());
    knit_with_fake_gh_env(
        &f.workspace,
        [
            "project",
            "add",
            "cloned",
            UPSTREAM,
            "--fork",
            "--owner",
            "contributor-org",
        ],
        &f.bin,
        &f.api,
        &[
            ("KNIT_GITHUB_API_TRANSPORT", "gh"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", &key),
            ("GIT_CONFIG_VALUE_0", UPSTREAM),
        ],
    );
    let cloned = registered_path(&f, "cloned");
    assert_eq!(
        git(&cloned, ["rev-parse", "HEAD"]),
        git(&f.local, ["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(&cloned, ["config", "--get", "remote.origin.url"]),
        UPSTREAM
    );
    assert_eq!(
        git(&cloned, ["config", "--get", "remote.origin.pushurl"]),
        "https://github.com/contributor-org/library.git"
    );
}

#[test]
fn contribution_discovery_recognizes_new_fork_and_upstream_review_target() {
    let f = Fixture::new();
    f.run(&["fork", "library"]);
    let mut repo: knit::model::RepoEntry = serde_json::from_value(json!({
        "id": "library", "path": f.local, "remote": UPSTREAM,
        "baseBranch": "main", "featureBranch": "contribution",
        "headSha": git(&f.local, ["rev-parse", "HEAD"])
    }))
    .unwrap();
    assert!(knit::contribution::discover(&f.local, &mut repo).unwrap());
    assert_eq!(repo.source_remote.as_deref(), Some(FORK));
    assert_eq!(repo.target_remote.as_deref(), Some(UPSTREAM));
    assert_eq!(
        knit::contribution::head(&repo, "contribution").unwrap(),
        "contributor:contribution"
    );
    let forge = knit::providers::by_id("github").unwrap();
    let target =
        knit::contribution::target(&f.local, &repo, forge.as_ref(), "main", false).unwrap();
    let identity = target.contribution.unwrap();
    assert_eq!(identity.source, "contributor/library");
    assert_eq!(identity.target, "upstream-org/library");
}

// Isolate provider environment from other tests in this process. The fake API
// deliberately rejects the final write; reaching it proves read-only publish
// preflight accepted the newly created fork without creating a review.
#[test]
fn publish_preflight_child() {
    let Ok(path) = std::env::var("FORK_CREATE_PREFLIGHT_REPO") else {
        return;
    };
    let local = PathBuf::from(path);
    let mut repo: knit::model::RepoEntry = serde_json::from_value(json!({
        "id": "library", "path": local, "remote": UPSTREAM,
        "baseBranch": "main", "featureBranch": "contribution",
        "headSha": git(&local, ["rev-parse", "HEAD"])
    }))
    .unwrap();
    assert!(knit::contribution::discover(&local, &mut repo).unwrap());
    let forge = knit::providers::by_id("github").unwrap();
    let target = knit::contribution::target(&local, &repo, forge.as_ref(), "main", false).unwrap();
    let error = forge
        .create(
            &target,
            "main",
            "contributor:contribution",
            "Contribution",
            "Synthetic test",
            false,
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("synthetic preflight complete"),
        "{error:#}"
    );
}

#[test]
fn publish_preflight_accepts_newly_created_direct_fork() {
    let f = Fixture::new();
    f.run(&["fork", "library"]);
    let sha = git(&f.local, ["rev-parse", "HEAD"]);
    f.settings(json!({"head": sha}));
    fs::write(f.api.join("calls"), "").unwrap();
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(f.bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "publish_preflight_child", "--nocapture"])
        .current_dir(&f.local)
        .env("FORK_CREATE_PREFLIGHT_REPO", &f.local)
        .env("PATH", path)
        .env("GH_FAKE_DIR", &f.api)
        .env("KNIT_GITHUB_API_TRANSPORT", "gh")
        .env("KNIT_HOME", f.root.join("preflight-home"))
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = f.calls();
    for endpoint in [
        "repos/contributor/library",
        "repos/upstream-org/library",
        "repos/upstream-org/library/git/ref/heads/main",
        "repos/contributor/library/git/ref/heads/contribution",
    ] {
        assert!(
            calls
                .iter()
                .any(|c| c["method"] == "GET" && c["endpoint"] == endpoint),
            "missing {endpoint}: {calls:?}"
        );
    }
    let attempted = calls.iter().find(|c| c["method"] == "POST").unwrap();
    assert_eq!(attempted["endpoint"], "repos/upstream-org/library/pulls");
    assert_eq!(attempted["payload"]["head"], "contributor:contribution");
    assert_eq!(attempted["payload"]["head_repo"], "library");
}

#[test]
fn timeout_is_bounded_and_never_writes_configuration() {
    let f = Fixture::new();
    f.settings(json!({"never_ready": true}));
    let before = (f.config(), f.project_bytes());
    let start = std::time::Instant::now();
    let (_, stderr, ok) = f.split(&["fork", "library"], &[]);
    assert!(!ok);
    assert!(stderr.contains("timed out"), "{stderr}");
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(f.post_count(), 1);
    assert_eq!((f.config(), f.project_bytes()), before);
}

#[test]
fn post_indirect_fork_is_rejected_before_configuration() {
    let f = Fixture::new();
    f.settings(json!({"post_indirect": true}));
    let before = (f.config(), f.project_bytes());
    let error = f.fail(&["fork", "library"]);
    assert!(error.contains("direct fork"), "{error}");
    assert_eq!(f.post_count(), 1);
    assert_eq!((f.config(), f.project_bytes()), before);
}

#[test]
fn human_output_distinguishes_created_from_existing_and_json_agrees() {
    let f = Fixture::new();
    assert_eq!(
        f.run(&["fork", "library"]).trim(),
        "library: upstream-org/library → contributor/library (forked), pushes go to the fork"
    );
    assert_eq!(
        f.run(&["fork", "library"]).trim(),
        "library: upstream-org/library → contributor/library (exists), pushes go to the fork"
    );
    let report: Value = serde_json::from_str(&f.run(&["fork", "library", "--json"])).unwrap();
    assert_eq!(report["repos"][0]["status"], "exists");
    assert_eq!(f.post_count(), 1);
}

#[test]
fn clone_destination_mismatch_refuses_without_changes() {
    let f = Fixture::new();
    let destination = f.workspace.join("cloned");
    init_repo(&destination, "unrelated");
    git(
        &destination,
        [
            "remote",
            "add",
            "origin",
            "https://github.com/other/library.git",
        ],
    );
    let before = (
        fs::read(destination.join(".git/config")).unwrap(),
        f.project_bytes(),
        git(&destination, ["rev-parse", "HEAD"]),
    );
    let url = format!("file://{}", f.upstream.display());
    let error = f.fail(&["project", "add", "cloned", &url, "--fork"]);
    assert!(error.contains("is not a clone of"), "{error}");
    assert_eq!(
        (
            fs::read(destination.join(".git/config")).unwrap(),
            f.project_bytes(),
            git(&destination, ["rev-parse", "HEAD"])
        ),
        before
    );
    assert!(f.calls().is_empty());
}

#[test]
fn same_clone_is_adopted_and_partial_clone_filter_is_preserved() {
    let f = Fixture::new();
    git(&f.upstream, ["config", "uploadpack.allowFilter", "true"]);
    let url = format!("file://{}", f.upstream.display());
    f.run(&["project", "add", "cloned", &url, "--no-fork"]);
    let cloned = registered_path(&f, "cloned");
    assert_eq!(
        git(&cloned, ["config", "--get", "remote.origin.promisor"]),
        "true"
    );
    assert_eq!(
        git(
            &cloned,
            ["config", "--get", "remote.origin.partialclonefilter"]
        ),
        "blob:none"
    );
    fs::write(cloned.join("local-only.txt"), "preserve local work").unwrap();
    let config = fs::read(cloned.join(".git/config")).unwrap();
    f.run(&["project", "add", "cloned", &url, "--no-fork"]);
    assert_eq!(registered_path(&f, "cloned"), cloned);
    assert_eq!(fs::read(cloned.join(".git/config")).unwrap(), config);
    assert_eq!(
        fs::read_to_string(cloned.join("local-only.txt")).unwrap(),
        "preserve local work"
    );
    let project: Value = serde_json::from_slice(&f.project_bytes()).unwrap();
    assert_eq!(
        project["repos"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["id"] == "cloned")
            .count(),
        1
    );
    assert!(f.calls().is_empty());
}

#[test]
fn public_fork_notice_occurs_once_before_first_post_for_multiple_repos() {
    let f = Fixture::new();
    let other = f.root.join("second");
    init_repo(&other, "second");
    git(&other, ["remote", "add", "origin", UPSTREAM]);
    knit(
        &f.workspace,
        ["project", "add", "second", other.to_str().unwrap()],
    );
    let notice = f.api.join("notice.log");
    let original = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(f.bin.clone()).chain(std::env::split_paths(&original)),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knit"))
        .args(["fork", "library", "second", "--json"])
        .current_dir(&f.workspace)
        .env("PATH", path)
        .env("GH_FAKE_DIR", &f.api)
        .env("KNIT_HOME", f.root.join("notice-home"))
        .env("KNIT_GITHUB_API_TRANSPORT", "gh")
        .env("FORK_CREATE_NOTICE_LOG", &notice)
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .stderr(fs::File::create(&notice).unwrap())
        .output()
        .unwrap();
    let stderr = fs::read_to_string(&notice).unwrap();
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "{stderr}"
    );
    assert!(stderr.to_lowercase().contains("public"), "{stderr}");
    assert_eq!(f.post_count(), 1);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["repos"].as_array().unwrap().len(), 2);
}

fn is_loopback_api(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some_and(|port| port != 0)
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
}

struct NativeServer(std::process::Child);
impl Drop for NativeServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_native_server(f: &Fixture) -> (NativeServer, String) {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let script = f.api.join("fork_server.py");
    fs::write(&script, include_str!("fixtures/fork_create_http.py")).unwrap();
    let mut server = NativeServer(
        Command::new(python_executable())
            .arg(&script)
            .arg(f.bin.join("gh.py"))
            .current_dir(&f.workspace)
            .env("GH_FAKE_DIR", &f.api)
            .env_remove("ELECTRON_RUN_AS_NODE")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut api = String::new();
    std::io::BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut api)
        .unwrap();
    let api = api.trim().to_owned();
    assert!(
        is_loopback_api(&api),
        "loopback fixture did not start: {api:?}"
    );
    assert!(
        server.0.try_wait().unwrap().is_none(),
        "loopback fixture exited"
    );
    (server, api)
}

#[test]
fn native_fixture_rejects_empty_external_and_malformed_api_urls() {
    for value in [
        "",
        "https://api.github.com",
        "http://127.0.0.1:0",
        "http://127.0.0.1:123@github.com",
        "http://127.0.0.1:123/path",
        "http://127.0.0.1:123?redirect=external",
        "http://localhost:123",
    ] {
        assert!(!is_loopback_api(value), "{value}");
    }
    assert!(is_loopback_api("http://127.0.0.1:12345"));
}

#[test]
fn native_api_uses_token_for_creation_and_polling() {
    let f = Fixture::new();
    f.settings(json!({"delay_once": true}));
    let (_server, api) = start_native_server(&f);
    let home = f.root.join("native-home");
    fs::create_dir_all(&home).unwrap();
    let (stdout, stderr, ok) = f.split(
        &["fork", "library", "--json"],
        &[
            ("KNIT_HOME", home.to_str().unwrap()),
            ("KNIT_GITHUB_API_TRANSPORT", "native"),
            ("KNIT_GITHUB_API_BASE", api.trim()),
            ("GH_TOKEN", "synthetic-upstream-token"),
            ("GITHUB_TOKEN", "synthetic-wrong-secondary"),
        ],
    );
    assert!(ok, "{stdout}{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["repos"][0]["status"], "forked");
    assert_eq!(f.pushurl(), FORK);
    assert_eq!(f.post_count(), 1);
    let calls = f.calls();
    let post = calls.iter().position(|c| c["method"] == "POST").unwrap();
    assert!(
        calls[post + 1..]
            .iter()
            .filter(|c| c["endpoint"] == "repos/contributor/library")
            .count()
            >= 2
    );
    let headers = fs::read_to_string(f.api.join("headers")).unwrap();
    assert!(!headers.is_empty());
    for line in headers.lines() {
        let value: Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["authorization"], "Bearer synthetic-upstream-token");
    }
}

#[test]
fn saved_upstream_token_overrides_ambient_for_all_gh_requests() {
    let f = Fixture::new();
    f.settings(json!({"expected_token": "synthetic-upstream-token", "delay_once": true}));
    let home = f.root.join("bound-home");
    fs::create_dir_all(&home).unwrap();
    let project_path = f.workspace.join(".knit/projects/demo.project.json");
    let mut project: Value = serde_json::from_slice(&f.project_bytes()).unwrap();
    project["repos"][0]["remote"] = json!(UPSTREAM);
    fs::write(&project_path, serde_json::to_vec(&project).unwrap()).unwrap();
    let key = fs::canonicalize(&project_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fs::write(home.join("forge-auth.json"), serde_json::to_vec(&json!({
        "credentials": {"upstream": {"provider": "github", "host": "github.com", "tokenEnv": "FORK_CREATE_TOKEN"}},
        "scopedCredentials": ["upstream"], "projects": {key: {"library": "upstream"}}
    })).unwrap()).unwrap();
    let (stdout, stderr, ok) = f.split(
        &["fork", "library", "--json"],
        &[
            ("KNIT_HOME", home.to_str().unwrap()),
            ("GH_TOKEN", "synthetic-wrong-ambient"),
            ("FORK_CREATE_TOKEN", "synthetic-upstream-token"),
        ],
    );
    assert!(ok, "{stdout}{stderr}");
    assert_eq!(f.pushurl(), FORK);
    assert_eq!(f.post_count(), 1);
}

#[test]
fn inherited_pushurl_force_refuses_include_and_global_before_post() {
    for included in [true, false] {
        let f = Fixture::new();
        let inherited = f.root.join("inherited.gitconfig");
        fs::write(
            &inherited,
            "[remote \"origin\"]\n\tpushurl = https://github.com/other/library.git\n",
        )
        .unwrap();
        if included {
            git(
                &f.local,
                [
                    "config",
                    "--local",
                    "include.path",
                    inherited.to_str().unwrap(),
                ],
            );
        }
        let before = (f.config(), f.project_bytes(), fs::read(&inherited).unwrap());
        let extra = if included {
            vec![]
        } else {
            vec![("GIT_CONFIG_GLOBAL", inherited.to_str().unwrap())]
        };
        let (stdout, stderr, ok) = f.split(&["fork", "library", "--force"], &extra);
        assert!(!ok, "{stdout}{stderr}");
        assert!(stderr.contains("outside this repository"), "{stderr}");
        f.no_posts();
        assert_eq!(
            (f.config(), f.project_bytes(), fs::read(&inherited).unwrap()),
            before
        );
    }
}

fn assert_no_pushurl(repo: &Path) {
    assert!(
        !git(repo, ["config", "--local", "--list"])
            .lines()
            .any(|line| line.starts_with("remote.origin.pushurl=")),
        "unexpected pushurl in {}",
        repo.display()
    );
}

fn assert_summary(stdout: &str, expected: &str) {
    assert_eq!(
        stdout.lines().filter(|line| *line == expected).count(),
        1,
        "{stdout}"
    );
}

fn assert_unknown_guidance(stderr: &str, id: &str, upstream: &str) {
    let guidance = format!(
        "Knit can't tell whether you can push to {upstream}; sign in with `knit auth` or pass --fork or --no-fork"
    );
    assert_eq!(stderr.matches(&guidance).count(), 1, "{stderr}");
    assert!(stderr.contains(&format!("{id}: {guidance}")), "{stderr}");
    assert!(!stderr.contains(&format!("{id}: {id}:")), "{stderr}");
}

#[test]
fn project_add_url_auto_uses_upstream_push_permission() {
    for push in [true, false] {
        let f = Fixture::new();
        f.settings(json!({"push": push}));
        let (stdout, stderr, ok) = f.clone_split(&["project", "add", "cloned", UPSTREAM]);
        assert!(ok, "{stdout}{stderr}");
        let cloned = registered_path(&f, "cloned");
        assert_eq!(
            git(&cloned, ["config", "--get", "remote.origin.url"]),
            UPSTREAM
        );
        assert!(f
            .calls()
            .iter()
            .any(|c| c["method"] == "GET" && c["endpoint"] == "repos/upstream-org/library"));
        if push {
            assert_summary(&stdout, "cloned: direct (you can push)");
            f.no_posts();
            assert_no_pushurl(&cloned);
        } else {
            assert_summary(&stdout, "cloned: via fork contributor/library");
            assert_eq!(f.post_count(), 1);
            assert_eq!(
                git(&cloned, ["config", "--get", "remote.origin.pushurl"]),
                FORK
            );
        }
    }
}

#[test]
fn project_add_url_unknown_permission_stays_added_with_explicit_choices() {
    let f = Fixture::new();
    let (stdout, stderr, ok) = f.clone_split(&["project", "add", "cloned", UPSTREAM]);
    assert!(!ok, "{stdout}{stderr}");
    assert_unknown_guidance(&stderr, "cloned", "upstream-org/library");
    let cloned = registered_path(&f, "cloned");
    assert_eq!(
        git(&cloned, ["rev-parse", "HEAD"]),
        git(&f.local, ["rev-parse", "HEAD"])
    );
    assert_no_pushurl(&cloned);
    f.no_posts();
    assert!(!f.calls().is_empty());
}

#[test]
fn project_add_no_fork_skips_all_forge_calls() {
    let f = Fixture::new();
    f.settings(json!({"push": false}));
    let (stdout, stderr, ok) = f.clone_split(&["project", "add", "cloned", UPSTREAM, "--no-fork"]);
    assert!(ok, "{stdout}{stderr}");
    assert_summary(&stdout, "cloned: direct (--no-fork)");
    assert_no_pushurl(&registered_path(&f, "cloned"));
    assert!(f.calls().is_empty());
}

#[test]
fn project_add_explicit_fork_overrides_push_permission() {
    let f = Fixture::new();
    f.settings(json!({"push": true}));
    let (stdout, stderr, ok) = f.clone_split(&["project", "add", "cloned", UPSTREAM, "--fork"]);
    assert!(ok, "{stdout}{stderr}");
    assert_eq!(f.post_count(), 1);
    assert_eq!(
        git(
            &registered_path(&f, "cloned"),
            ["config", "--get", "remote.origin.pushurl"]
        ),
        FORK
    );
}

#[test]
fn project_add_local_defaults_to_never_even_without_push_permission() {
    let f = Fixture::new();
    f.settings(json!({"push": false}));
    f.run(&["project", "add", "local-copy", f.local.to_str().unwrap()]);
    assert_eq!(
        fs::canonicalize(registered_path(&f, "local-copy")).unwrap(),
        fs::canonicalize(&f.local).unwrap()
    );
    assert!(f.calls().is_empty());
    assert_no_pushurl(&f.local);
}

#[test]
fn fork_flags_conflict_before_project_or_clone_creation() {
    for flags in [
        vec!["--fork", "--no-fork"],
        vec!["--owner", "contributor-org", "--no-fork"],
    ] {
        for init in [false, true] {
            let f = Fixture::fresh_workspace();
            let mut args = if init {
                vec!["init", "demo", "upstream-org/library"]
            } else {
                vec!["project", "add", "cloned", UPSTREAM]
            };
            args.extend(flags.iter().copied());
            let (stdout, stderr, ok) = f.clone_split(&args);
            assert!(!ok, "{stdout}{stderr}");
            assert!(stderr.contains("--no-fork"), "{stderr}");
            assert!(stderr.contains("cannot be used with"), "{stderr}");
            assert!(!f.workspace.join(".knit").exists());
            assert!(!f.workspace.join("cloned").exists());
            assert!(f.calls().is_empty());
        }
    }
}

#[test]
fn init_repo_references_accept_shorthand_url_and_slugged_explicit_id() {
    for (reference, id) in [
        ("upstream-org/library", "library"),
        (UPSTREAM, "library"),
        ("Client Library=upstream-org/library", "client-library"),
        (
            "client-library=https://github.com/upstream-org/library.git",
            "client-library",
        ),
    ] {
        let f = Fixture::fresh_workspace();
        f.settings(json!({"push": true}));
        let (stdout, stderr, ok) = f.clone_split(&["init", "Demo", reference]);
        assert!(ok, "{reference}: {stdout}{stderr}");
        let repo = registered_path(&f, id);
        assert_eq!(
            git(&repo, ["rev-parse", "HEAD"]),
            git(&f.local, ["rev-parse", "HEAD"])
        );
        assert_no_pushurl(&repo);
        f.no_posts();
        assert!(!f.calls().is_empty());
        let project: Value = serde_json::from_slice(&f.project_bytes()).unwrap();
        assert_eq!(project["repos"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn init_multiple_repos_adds_both_and_reports_in_argument_order() {
    for second_push in [true, false] {
        let f = Fixture::fresh_workspace();
        init_remote_repo(&f.root, "second");
        f.settings(json!({"repos": {"library": {"push": false}, "second": {"push": second_push}}}));
        let (stdout, stderr, ok) = f.clone_split(&[
            "init",
            "demo",
            "upstream-org/library",
            "https://github.com/upstream-org/second.git",
            "--agents",
        ]);
        assert!(ok, "{stdout}{stderr}");
        let library = registered_path(&f, "library");
        let second = registered_path(&f, "second");
        assert_eq!(
            git(&library, ["config", "--get", "remote.origin.pushurl"]),
            FORK
        );
        if second_push {
            assert_no_pushurl(&second);
        } else {
            assert_eq!(
                git(&second, ["config", "--get", "remote.origin.pushurl"]),
                "https://github.com/contributor/second.git"
            );
        }
        assert_eq!(f.post_count(), if second_push { 1 } else { 2 });
        let first = stdout.find("library:").expect(&stdout);
        let second_summary = stdout.find("second:").expect(&stdout);
        assert!(first < second_summary, "{stdout}");
        assert_summary(&stdout, "library: via fork contributor/library");
        assert_summary(
            &stdout,
            if second_push {
                "second: direct (you can push)"
            } else {
                "second: via fork contributor/second"
            },
        );
        let agents = fs::read_to_string(f.workspace.join("AGENTS.md")).unwrap();
        assert!(agents.lines().any(|line| line == "- `library`"), "{agents}");
        assert!(agents.lines().any(|line| line == "- `second`"), "{agents}");
        let project: Value = serde_json::from_slice(&f.project_bytes()).unwrap();
        assert_eq!(project["repos"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn init_unknown_permission_keeps_repo_and_processes_following_repo() {
    let f = Fixture::fresh_workspace();
    init_remote_repo(&f.root, "second");
    f.settings(json!({"repos": {"second": {"push": false}}}));
    let (stdout, stderr, ok) = f.clone_split(&[
        "init",
        "demo",
        "upstream-org/library",
        "upstream-org/second",
    ]);
    assert!(!ok, "{stdout}{stderr}");
    assert_unknown_guidance(&stderr, "library", "upstream-org/library");
    assert_no_pushurl(&registered_path(&f, "library"));
    assert_eq!(
        git(
            &registered_path(&f, "second"),
            ["config", "--get", "remote.origin.pushurl"]
        ),
        "https://github.com/contributor/second.git"
    );
    assert_eq!(f.post_count(), 1);
    assert_summary(&stdout, "second: via fork contributor/second");
}

#[test]
fn init_no_fork_skips_forge_and_explicit_fork_honors_owner() {
    for force_fork in [false, true] {
        let f = Fixture::fresh_workspace();
        f.settings(json!({"push": true, "owner": "contributor-org"}));
        let mut args = vec!["init", "demo", "upstream-org/library"];
        if force_fork {
            args.extend(["--fork", "--owner", "contributor-org"]);
        } else {
            args.push("--no-fork");
        }
        let (stdout, stderr, ok) = f.clone_split(&args);
        assert!(ok, "{stdout}{stderr}");
        let repo = registered_path(&f, "library");
        if force_fork {
            assert_eq!(
                git(&repo, ["config", "--get", "remote.origin.pushurl"]),
                "https://github.com/contributor-org/library.git"
            );
            let calls = f.calls();
            let post = calls.iter().find(|c| c["method"] == "POST").unwrap();
            assert_eq!(post["payload"]["organization"], "contributor-org");
        } else {
            assert!(f.calls().is_empty());
            assert_no_pushurl(&repo);
        }
    }
}

#[test]
fn init_invalid_references_and_duplicate_ids_fail_before_creating_anything() {
    for refs in [
        vec!["upstream-org/library", "other-org/library"],
        vec![
            "Client Library=upstream-org/library",
            "client-library=upstream-org/second",
        ],
        vec!["upstream-org/library", "invalid-reference"],
        vec!["upstream-org/library", "=upstream-org/second"],
        vec!["upstream-org/library", "second="],
        vec!["upstream-org/library", "owner/name/extra"],
        vec!["upstream-org/library", "https://"],
        vec!["upstream-org/library", "https://github.com"],
    ] {
        let f = Fixture::fresh_workspace();
        let mut args = vec!["init", "demo"];
        args.extend(refs.iter().copied());
        let (stdout, stderr, ok) = f.clone_split(&args);
        assert!(!ok, "{refs:?}: {stdout}{stderr}");
        assert!(
            !f.workspace.join(".knit").exists(),
            "{refs:?}: {stdout}{stderr}"
        );
        assert_eq!(fs::read_dir(&f.workspace).unwrap().count(), 0);
        assert!(f.calls().is_empty());
    }
}

#[test]
fn plain_init_still_creates_empty_project_without_forge_calls() {
    let f = Fixture::fresh_workspace();
    f.run(&["init", "demo"]);
    let project: Value = serde_json::from_slice(&f.project_bytes()).unwrap();
    assert!(project["repos"].as_array().unwrap().is_empty());
    assert!(f.calls().is_empty());
    assert!(!f.workspace.join("library").exists());
}

#[test]
fn project_add_auto_unsupported_forge_keeps_clone_without_forge_calls() {
    let f = Fixture::new();
    let url = format!("file://{}", f.upstream.display());
    let (stdout, stderr, ok) = f.split(&["project", "add", "cloned", &url], &[]);
    assert!(!ok, "{stdout}{stderr}");
    assert_unknown_guidance(&stderr, "cloned", "cloned");
    let cloned = registered_path(&f, "cloned");
    assert_eq!(git(&cloned, ["config", "--get", "remote.origin.url"]), url);
    assert_eq!(
        git(&cloned, ["rev-parse", "HEAD"]),
        git(&f.local, ["rev-parse", "HEAD"])
    );
    assert_no_pushurl(&cloned);
    assert!(f.calls().is_empty());
}

#[test]
fn project_add_auto_missing_credentials_preserves_clone_with_guidance() {
    let f = Fixture::new();
    f.settings(json!({"missing_credentials": true}));
    let (stdout, stderr, ok) = f.clone_split(&["project", "add", "cloned", UPSTREAM]);
    assert!(!ok, "{stdout}{stderr}");
    assert_unknown_guidance(&stderr, "cloned", "upstream-org/library");
    assert!(stderr.contains("gh auth login"), "{stderr}");
    assert_no_pushurl(&registered_path(&f, "cloned"));
    assert!(!f.calls().is_empty());
    f.no_posts();
}

#[test]
fn init_failed_first_clone_still_adds_and_summarizes_following_repo() {
    let f = Fixture::fresh_workspace();
    f.settings(json!({"push": true}));
    let (stdout, stderr, ok) = f.clone_split(&[
        "init",
        "demo",
        "upstream-org/missing",
        "upstream-org/library",
    ]);
    assert!(!ok, "{stdout}{stderr}");
    assert!(stderr.contains("missing:"), "{stderr}");
    assert_summary(&stdout, "library: direct (you can push)");
    let project: Value = serde_json::from_slice(&f.project_bytes()).unwrap();
    assert_eq!(project["repos"].as_array().unwrap().len(), 1);
    assert_eq!(project["repos"][0]["id"], "library");
    assert_no_pushurl(&registered_path(&f, "library"));
    assert!(!f.workspace.join("missing").exists());
    assert!(f
        .calls()
        .iter()
        .all(|call| !call["endpoint"].as_str().unwrap().contains("missing")));
    f.no_posts();
}

#[test]
fn auto_fork_accepts_owner_without_explicit_fork_flag() {
    for init in [false, true] {
        let f = if init {
            Fixture::fresh_workspace()
        } else {
            Fixture::new()
        };
        f.settings(json!({"push": false, "owner": "contributor-org"}));
        let mut args = if init {
            vec!["init", "demo", "upstream-org/library"]
        } else {
            vec!["project", "add", "library-copy", UPSTREAM]
        };
        args.extend(["--owner", "contributor-org"]);
        let (stdout, stderr, ok) = f.clone_split(&args);
        assert!(ok, "{stdout}{stderr}");
        let id = if init { "library" } else { "library-copy" };
        assert_summary(&stdout, &format!("{id}: via fork contributor-org/library"));
        assert_eq!(
            git(
                &registered_path(&f, id),
                ["config", "--get", "remote.origin.pushurl"]
            ),
            "https://github.com/contributor-org/library.git"
        );
        assert_eq!(f.post_count(), 1);
        let calls = f.calls();
        let post = calls.iter().find(|call| call["method"] == "POST").unwrap();
        assert_eq!(post["payload"]["organization"], "contributor-org");
    }
}

fn hosted_export(ids: &[&str]) -> String {
    let url = |id: &str| format!("https://github.com/upstream-org/{id}.git");
    let membership: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "path": "", "remote": url(id), "baseBranch": "main"}))
        .collect();
    let repositories: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({"localId": id, "name": id, "remoteUrl": url(id), "defaultBranch": "main",
                   "visibility": "public", "metadata": {}})
        })
        .collect();
    json!({"data": {
        "project": {"slug": "demo"},
        "knitProject": {"schemaVersion": "0.1", "kind": "KnitProject", "id": "demo",
                        "createdAt": "2026-01-01T00:00:00.000Z",
                        "updatedAt": "2026-01-01T00:00:00.000Z", "repos": membership},
        "repositories": repositories,
        "omittedRepositoryCount": 0,
        "bundles": [],
        "historyEvents": [],
    }})
    .to_string()
}

fn clone_hosted(f: &Fixture, extra: &[&str]) -> (Value, String, PathBuf) {
    let base_url = spawn_fake_remote_with_body(hosted_export(&["library"]));
    let target = f.root.join("contributor-checkout");
    let mut args = vec![
        "clone",
        "acme/demo",
        target.to_str().unwrap(),
        "--remote",
        "hosted",
        "--url",
        &base_url,
        "--no-worktree",
        "--json",
    ];
    args.extend_from_slice(extra);
    let (stdout, stderr, ok) = f.clone_split(&args);
    assert!(ok, "{stdout}{stderr}");
    let document = serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("{stdout}"));
    (document, stderr, target)
}

fn cloned_project(target: &Path) -> Value {
    serde_json::from_slice(&fs::read(target.join(".knit/projects/demo.project.json")).unwrap())
        .unwrap()
}

#[test]
fn clone_forks_only_repositories_you_cannot_push_to() {
    for push in [true, false] {
        let f = Fixture::fresh_workspace();
        f.settings(json!({"push": push}));
        let (document, _, target) = clone_hosted(&f, &[]);
        let checkout = target.join("library");
        let expected = if push {
            json!({"id": "library", "status": "cloned", "push": "direct"})
        } else {
            json!({"id": "library", "status": "cloned", "push": "fork",
                   "fork": "contributor/library"})
        };
        assert_eq!(document["repos"], json!([expected]));
        if push {
            assert_no_pushurl(&checkout);
            f.no_posts();
        } else {
            assert_eq!(
                git(&checkout, ["config", "--get", "remote.origin.pushurl"]),
                FORK
            );
            assert_eq!(f.post_count(), 1);
        }
        // The fork is this person's push target only: the project, and so
        // what `knit sync push` uploads, keeps the upstream fetch URL.
        assert_eq!(
            git(&checkout, ["config", "--get", "remote.origin.url"]),
            UPSTREAM
        );
        assert_eq!(cloned_project(&target)["repos"][0]["remote"], UPSTREAM);
    }
}

#[test]
fn clone_with_unknown_permission_keeps_the_checkout_and_names_knit_fork() {
    let f = Fixture::fresh_workspace();
    f.settings(json!({"missing_credentials": true}));
    let (document, stderr, target) = clone_hosted(&f, &[]);
    assert_eq!(
        document["repos"],
        json!([{"id": "library", "status": "cloned", "push": "unknown"}])
    );
    assert!(
        stderr.contains(
            "library: Knit can't tell whether you can push to upstream-org/library. If you can't, run `knit fork library`."
        ),
        "{stderr}"
    );
    assert!(target.join("library/app.txt").exists());
    assert_no_pushurl(&target.join("library"));
    f.no_posts();
}

#[test]
fn clone_no_fork_skips_the_forge_and_fork_honors_owner() {
    let f = Fixture::fresh_workspace();
    f.settings(json!({"push": false}));
    let (document, _, target) = clone_hosted(&f, &["--no-fork"]);
    assert_eq!(document["repos"][0]["push"], "direct");
    assert_no_pushurl(&target.join("library"));
    assert!(f.calls().is_empty());

    let f = Fixture::fresh_workspace();
    f.settings(json!({"push": true, "owner": "contributor-org"}));
    let (document, _, target) = clone_hosted(&f, &["--fork", "--owner", "contributor-org"]);
    assert_eq!(document["repos"][0]["fork"], "contributor-org/library");
    assert_eq!(
        git(
            &target.join("library"),
            ["config", "--get", "remote.origin.pushurl"]
        ),
        "https://github.com/contributor-org/library.git"
    );
    let calls = f.calls();
    let post = calls.iter().find(|call| call["method"] == "POST").unwrap();
    assert_eq!(post["payload"]["organization"], "contributor-org");
}

#[test]
fn sync_pull_forks_new_members_and_leaves_existing_checkouts_alone() {
    let f = Fixture::new();
    init_remote_repo(&f.root, "second");
    f.settings(json!({"repos": {"library": {"push": false}, "second": {"push": false}}}));
    let base_url = spawn_fake_remote_with_body(hosted_export(&["library", "second"]));
    f.run(&["remote", "add", "hosted", &base_url]);
    let key = format!("url.file://{}/.insteadOf", f.root.display());
    let (stdout, stderr, ok) = f.split(
        &["sync", "pull", "--bundles", "--repos"],
        &[
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", &key),
            ("GIT_CONFIG_VALUE_0", "https://github.com/upstream-org/"),
            ("KNIT_REMOTE_TOKEN", "test-token"),
        ],
    );
    assert!(ok, "{stdout}{stderr}");
    assert_summary(&stdout, "second: via fork contributor/second");
    assert_eq!(
        git(
            &registered_path(&f, "second"),
            ["config", "--get", "remote.origin.pushurl"]
        ),
        "https://github.com/contributor/second.git"
    );
    assert_no_pushurl(&f.local);
    assert!(f
        .calls()
        .iter()
        .all(|call| !call["endpoint"].as_str().unwrap().contains("/library")));
}
