mod common;
use common::*;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

fn write(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    target: PathBuf,
    checkout: PathBuf,
    bundle: PathBuf,
    project: PathBuf,
    api: String,
    wrong_head: Arc<Mutex<bool>>,
    read_only: Arc<Mutex<bool>>,
    maintainer_merged: Arc<Mutex<bool>>,
}
impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let (target, checkout, collaborator) = init_remote_repo(&root, "service");
        let source = root.join("fork.git");
        git(
            &root,
            [
                "clone",
                "--bare",
                target.to_str().unwrap(),
                source.to_str().unwrap(),
            ],
        );
        git(
            &checkout,
            [
                "remote",
                "set-url",
                "origin",
                "https://github.com/contributor/service.git",
            ],
        );
        for (url, path) in [
            ("https://github.com/contributor/service.git", &source),
            ("https://github.com/upstream/service.git", &target),
        ] {
            git(
                &checkout,
                ["config", &format!("url.{}.insteadOf", path.display()), url],
            );
        }
        let base = git(&checkout, ["rev-parse", "HEAD"]).trim().to_owned();
        git(&checkout, ["checkout", "-b", "feature"]);
        fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
        git(&checkout, ["add", "."]);
        git(&checkout, ["commit", "-m", "Synthetic feature"]);
        git(&checkout, ["push", "origin", "feature"]);
        let head = git(&checkout, ["rev-parse", "HEAD"]).trim().to_owned();
        fs::write(collaborator.join("base.txt"), "target base\n").unwrap();
        git(&collaborator, ["add", "."]);
        git(&collaborator, ["commit", "-m", "Target base"]);
        git(
            &collaborator,
            ["push", "origin", "HEAD:main", "HEAD:staging"],
        );
        let bundle = root.join(".knit/bundles/demo.bundle.json");
        let mut b = serde_json::to_value(knit::model::ChangeGroup::new(
            "demo".into(),
            "Synthetic fork lifecycle".into(),
            "2026-01-01T00:00:00Z".into(),
        ))
        .unwrap();
        b["repos"] = json!([{"id":"service","path":checkout,"worktreePath":checkout,"baseBranch":"main","baseSha":base,"headSha":head,"featureBranch":"feature","remote":"https://github.com/contributor/service.git","sourceRemote":"https://github.com/contributor/service.git","targetRemote":"https://github.com/upstream/service.git"}]);
        b["publications"] = json!([{"repoId":"service","provider":"github","kind":"pull_request","number":7,"url":"https://github.com/upstream/service/pull/7","baseBranch":"main","headBranch":"feature","state":"OPEN","title":"Synthetic review","createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z"}]);
        write(&bundle, &b);
        write(
            &root.join(".knit/config.json"),
            &json!({"schemaVersion":"0.1","kind":"KnitConfig","activeBundle":"demo"}),
        );
        let project = root.join("project.json");
        let mut p = serde_json::to_value(knit::model::KnitProject::new(
            root.file_name().unwrap().to_str().unwrap().into(),
            "2026-01-01T00:00:00Z".into(),
        ))
        .unwrap();
        p["repos"] = json!([{"id":"service","path":checkout,"baseBranch":"main","remote":"https://github.com/upstream/service.git"}]);
        p["landing"] = json!({"preflight":{"mergeability":"all"}});
        write(&project, &p);
        write(&root.join("roots.json"), &json!({"service":checkout}));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let wrong_head = Arc::new(Mutex::new(false));
        let wrong = wrong_head.clone();
        let read_only = Arc::new(Mutex::new(false));
        let no_push = read_only.clone();
        let maintainer_merged = Arc::new(Mutex::new(false));
        let maintainer = maintainer_merged.clone();
        let fork = source.clone();
        let api_log = root.join("api.log");
        std::thread::spawn(move || {
            let mut base = "main".to_owned();
            let mut merged = false;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut bytes = vec![];
                let mut buf = [0; 8192];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let len = header
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|n| n.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let req = String::from_utf8_lossy(&bytes);
                let line = req.lines().next().unwrap_or("");
                let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                let mut log = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&api_log)
                    .unwrap();
                writeln!(log, "{line} {body}").unwrap();
                let response = if line.starts_with("GET /repos/upstream/service ") {
                    if *no_push.lock().unwrap() {
                        json!({"full_name":"upstream/service","permissions":{"push":false}})
                    } else {
                        json!([])
                    }
                } else if line.starts_with("PATCH ") {
                    let payload: Value = serde_json::from_str(body).unwrap();
                    base = payload["base"].as_str().unwrap().into();
                    json!({})
                } else if line.starts_with("PUT ") {
                    merged = true;
                    json!({"merged":true})
                } else if line.contains("/pulls/7 ") {
                    let head = git(&fork, ["rev-parse", "feature"]).trim().to_owned();
                    let merged = merged || *maintainer.lock().unwrap();
                    json!({"number":7,"html_url":"https://github.com/upstream/service/pull/7","state":if merged {"closed"}else{"open"},"merged":merged,"merge_commit_sha":head,"draft":false,"mergeable":true,"mergeable_state":"clean","head":{"ref":"feature","sha":head,"repo":{"full_name":if *wrong.lock().unwrap(){"collision/service"}else{"contributor/service"}}},"base":{"ref":base,"repo":{"full_name":"upstream/service"}}})
                } else if line.contains("check-runs") {
                    json!({"check_runs":[]})
                } else if line.contains("/status") {
                    json!({"state":"success","statuses":[]})
                } else {
                    json!([])
                };
                let body = response.to_string();
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            }
        });
        Self {
            root,
            source,
            target,
            checkout,
            bundle,
            project,
            api,
            wrong_head,
            read_only,
            maintainer_merged,
        }
    }
    fn cmd(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_knit"))
            .current_dir(&self.root)
            .args(args)
            .env_remove("KNIT_BUNDLE")
            .env_remove("KNIT_SESSION")
            .env("KNIT_HOME", self.root.join("home"))
            .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
            .env("GH_TOKEN", "synthetic")
            .env("KNIT_GITHUB_API_TRANSPORT", "native")
            .env("KNIT_GITHUB_API_BASE", &self.api)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let o = self.cmd(args);
        assert!(
            o.status.success(),
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }
    /// The target repository refuses this account a merge, so landing waits
    /// for its maintainers.
    fn maintainers_merge(&self) {
        *self.read_only.lock().unwrap() = true;
        let mut project = read(&self.project);
        project["landing"] = json!({});
        let project_id = project["id"].as_str().unwrap().to_owned();
        write(
            &self
                .root
                .join(format!(".knit/projects/{project_id}.project.json")),
            &project,
        );
        let mut bundle = read(&self.bundle);
        bundle["projectId"] = json!(project_id);
        write(&self.bundle, &bundle);
    }
    /// A maintainer replays the contribution onto the newest upstream main and
    /// force-pushes it to the contributor's branch, as maintainer edits allow.
    fn maintainer_rebases(&self, extra: Option<&str>) -> String {
        let work = self.root.join("maintainer");
        git(
            &self.root,
            [
                "clone",
                self.target.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        git(&work, ["config", "user.email", "maintainer@example.test"]);
        git(&work, ["config", "user.name", "Maintainer"]);
        git(&work, ["fetch", self.source.to_str().unwrap(), "feature"]);
        git(&work, ["cherry-pick", "FETCH_HEAD"]);
        if let Some(text) = extra {
            fs::write(work.join("feature.txt"), text).unwrap();
            git(&work, ["commit", "--amend", "--no-edit", "-a"]);
        }
        git(
            &work,
            [
                "push",
                "--force",
                self.source.to_str().unwrap(),
                "HEAD:feature",
            ],
        );
        git(&work, ["rev-parse", "HEAD"]).trim().to_owned()
    }
    fn maintainer_merges(&self) {
        git(
            &self.source,
            ["push", self.target.to_str().unwrap(), "feature:main"],
        );
        *self.maintainer_merged.lock().unwrap() = true;
    }
    fn run_path(&self) -> PathBuf {
        fs::read_dir(self.root.join(".knit/land-runs"))
            .unwrap()
            .map(|p| p.unwrap().path())
            .find(|p| p.extension().is_some_and(|s| s == "json"))
            .unwrap()
    }
    fn plan(&self, target: Option<&str>) {
        let mut args = vec![
            "land",
            "plan",
            "--from-artifact",
            self.bundle.to_str().unwrap(),
            "--project-file",
            self.project.to_str().unwrap(),
            "--out",
            "plan.json",
        ];
        if let Some(target) = target {
            args.extend(["--target", target]);
        }
        self.ok(&args);
    }
}

#[test]
fn update_fetches_target_pushes_source_and_refreshes_exact_new_head() {
    let f = Fixture::new();
    let target_before = git(&f.target, ["rev-parse", "main"]);
    f.ok(&["land", "update", "--push"]);
    let head = git(&f.checkout, ["rev-parse", "HEAD"]).trim().to_owned();
    assert_eq!(git(&f.source, ["rev-parse", "feature"]).trim(), head);
    assert_eq!(read(&f.bundle)["repos"][0]["headSha"], head);
    assert!(git_success(
        &f.checkout,
        ["merge-base", "--is-ancestor", target_before.trim(), "HEAD"]
    ));
    assert_eq!(git(&f.target, ["rev-parse", "main"]), target_before);
    assert!(!git_success(
        &f.target,
        ["show-ref", "--verify", "refs/heads/feature"]
    ));
}

#[test]
fn saved_plan_pins_both_identities_and_rejects_wrong_repository_collision() {
    let f = Fixture::new();
    f.plan(None);
    let plan = read(&f.root.join("plan.json"));
    assert_eq!(
        plan["repositoryIdentities"]["service"]["sourceRemote"],
        "https://github.com/contributor/service.git"
    );
    assert_eq!(
        plan["repositoryIdentities"]["service"]["targetRemote"],
        "https://github.com/upstream/service.git"
    );
    let mut unpinned = plan.clone();
    unpinned
        .as_object_mut()
        .unwrap()
        .remove("repositoryIdentities");
    write(&f.root.join("unpinned.json"), &unpinned);
    let unpinned_result = f.cmd(&[
        "land",
        "validate",
        "--plan",
        "unpinned.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
    ]);
    assert!(!unpinned_result.status.success());
    *f.wrong_head.lock().unwrap() = true;
    let o = f.cmd(&[
        "land",
        "apply",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--run-out",
        "run.json",
        "--out",
        "out.json",
    ]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("contradicts"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let mut bundle = read(&f.bundle);
    bundle["repos"][0]["sourceRemote"] = json!("https://github.com/collision/service.git");
    write(&f.bundle, &bundle);
    let o = f.cmd(&[
        "land",
        "validate",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
    ]);
    assert!(!o.status.success());
}

#[test]
fn saved_plan_merges_source_into_target_without_pushing_target_to_fork() {
    let f = Fixture::new();
    f.plan(Some("staging"));
    let fork_main = git(&f.source, ["rev-parse", "main"]);
    f.ok(&[
        "land",
        "apply",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--run-out",
        "run.json",
        "--out",
        "out.json",
    ]);
    let feature = git(&f.source, ["rev-parse", "feature"]);
    assert!(git_success(
        &f.target,
        ["merge-base", "--is-ancestor", feature.trim(), "staging"]
    ));
    assert_eq!(git(&f.source, ["rev-parse", "main"]), fork_main);
    assert!(!git_success(
        &f.source,
        ["show-ref", "--verify", "refs/heads/staging"]
    ));
    assert_eq!(
        read(&f.root.join("run.json"))["plan"],
        read(&f.root.join("plan.json"))
    );
}

#[test]
fn artifact_retarget_verifies_old_base_then_merges_on_target_new_base() {
    let f = Fixture::new();
    f.ok(&[
        "land",
        "--schema-version",
        "0.1",
        "apply",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--target",
        "staging",
        "--terminal",
        "--out",
        "out.json",
    ]);
    assert_eq!(
        read(&f.root.join("out.json"))["publications"][0]["baseBranch"],
        "staging"
    );
}

#[test]
fn saved_plan_localizes_feature_from_source_in_a_target_only_runner() {
    let f = Fixture::new();
    f.plan(Some("staging"));
    let runner = f.root.join("runner");
    git(
        &f.root,
        [
            "clone",
            f.target.to_str().unwrap(),
            runner.to_str().unwrap(),
        ],
    );
    configure_git_user(&runner);
    for (url, path) in [
        ("https://github.com/contributor/service.git", &f.source),
        ("https://github.com/upstream/service.git", &f.target),
    ] {
        git(
            &runner,
            ["config", &format!("url.{}.insteadOf", path.display()), url],
        );
    }
    let head = git(&f.source, ["rev-parse", "feature"]).trim().to_owned();
    assert!(!git_success(&runner, ["cat-file", "-e", &head]));
    // Same branch name upstream contains unrelated work: it cannot substitute for the fork.
    git(&f.target, ["branch", "feature", "main"]);
    write(&f.root.join("roots.json"), &json!({"service":runner}));
    f.ok(&[
        "land",
        "apply",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--run-out",
        "run.json",
        "--out",
        "out.json",
    ]);
    assert!(git_success(
        &f.target,
        ["merge-base", "--is-ancestor", &head, "staging"]
    ));
    assert_eq!(
        read(&f.root.join("run.json"))["sourceBundle"]["repos"][0]["sourceRemote"],
        "https://github.com/contributor/service.git"
    );
}

#[test]
fn standalone_integration_source_uses_saved_fork_identity_and_preserves_it() {
    let f = Fixture::new();
    f.plan(Some("staging"));
    git(&f.target, ["branch", "feature", "main"]);
    f.ok(&[
        "land",
        "source",
        "--plan",
        "plan.json",
        "--repo",
        "service",
        "--branch",
        "feature",
        "--repo-root",
        f.checkout.to_str().unwrap(),
        "--out",
        "source.json",
    ]);
    let plan = read(&f.root.join("plan.json"));
    let source = read(&f.root.join("source.json"));
    assert_eq!(source["repositoryIdentities"], plan["repositoryIdentities"]);
    assert_eq!(source["bundleFingerprint"], plan["bundleFingerprint"]);
    assert_eq!(
        source["integrationSources"]["service"]["sha"],
        plan["bundleHeads"]["service"]
    );
}

#[test]
fn readiness_accepts_exact_fork_identity_and_rejects_head_repository_collision() {
    let f = Fixture::new();
    f.ok(&["land", "check"]);
    *f.wrong_head.lock().unwrap() = true;
    let result = f.cmd(&["land", "check"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(text.contains("contradicts"), "{text}");
}

#[test]
fn native_saved_plan_retargets_and_merges_fork_review_on_target() {
    let f = Fixture::new();
    let mut project = read(&f.project);
    project["landing"]["targets"] = json!({"staging":{"terminal":true}});
    write(&f.project, &project);
    f.plan(Some("staging"));
    f.ok(&[
        "land",
        "apply",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--run-out",
        "run.json",
        "--out",
        "out.json",
    ]);
    assert_eq!(
        read(&f.root.join("out.json"))["publications"][0]["baseBranch"],
        "staging"
    );
    assert_eq!(
        read(&f.root.join("run.json"))["plan"],
        read(&f.root.join("plan.json"))
    );
}

#[test]
fn merge_command_routes_persisted_push_to_target() {
    let f = Fixture::new();
    f.ok(&["merge", "demo", "--into", "staging", "--fetch"]);
    f.ok(&["merge", "push"]);
    let head = git(&f.source, ["rev-parse", "feature"]);
    assert!(git_success(
        &f.target,
        ["merge-base", "--is-ancestor", head.trim(), "staging"]
    ));
    assert!(!git_success(
        &f.source,
        ["show-ref", "--verify", "refs/heads/staging"]
    ));
}

#[test]
fn recovery_in_a_fresh_target_only_runner_fetches_pinned_source_from_fork() {
    let f = Fixture::new();
    let head = git(&f.source, ["rev-parse", "feature"]).trim().to_owned();
    let mut project = read(&f.project);
    project["landing"] = json!({"merge":{"enabled":false},"onFailure":"stop","steps":[{
        "id":"deploy","type":"run","repoId":"service","effect":"deployment",
        "command":["python3","-c","raise SystemExit(7)"],
        "recovery":{"mode":"command","idempotent":true,
            "capture":{"command":["python3","-c","import json;print(json.dumps({'state':'previous'}))"]},
            "command":["git","cat-file","-e",head],"verify":{"command":["git","cat-file","-e",head]}}
    }]});
    write(&f.project, &project);
    f.plan(None);
    let result = f.cmd(&[
        "land",
        "apply",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--run-out",
        "run.json",
        "--out",
        "out.json",
    ]);
    assert!(!result.status.success());
    assert!(
        f.root.join("run.json").exists(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    // Remove only fixture-owned disposable execution worktrees. Recovery must recreate them.
    for path in fs::read_dir(f.root.join("run.checkouts")).unwrap() {
        git(
            &f.checkout,
            [
                "worktree",
                "remove",
                "--force",
                path.unwrap().path().to_str().unwrap(),
            ],
        );
    }
    let runner = f.root.join("recovery-runner");
    git(
        &f.root,
        [
            "clone",
            f.target.to_str().unwrap(),
            runner.to_str().unwrap(),
        ],
    );
    configure_git_user(&runner);
    for (url, path) in [
        ("https://github.com/contributor/service.git", &f.source),
        ("https://github.com/upstream/service.git", &f.target),
    ] {
        git(
            &runner,
            ["config", &format!("url.{}.insteadOf", path.display()), url],
        );
    }
    assert!(!git_success(&runner, ["cat-file", "-e", &head]));
    write(
        &f.root.join("recovery-roots.json"),
        &json!({"service":runner}),
    );
    f.ok(&[
        "land",
        "recover",
        "--run",
        "run.json",
        "--repo-roots",
        "recovery-roots.json",
        "--apply",
    ]);
    let run = read(&f.root.join("run.json"));
    assert_eq!(run["serviceStatus"], "restored");
    assert_eq!(run["plan"], read(&f.root.join("plan.json")));
    assert!(git_success(&runner, ["cat-file", "-e", &head]));
}

#[test]
fn local_update_can_be_pushed_on_a_later_invocation() {
    let f = Fixture::new();
    let before = git(&f.source, ["rev-parse", "feature"]);
    f.ok(&["land", "update"]);
    assert_eq!(git(&f.source, ["rev-parse", "feature"]), before);
    f.ok(&["land", "update", "--push"]);
    assert_eq!(
        git(&f.source, ["rev-parse", "feature"]),
        git(&f.checkout, ["rev-parse", "HEAD"])
    );
}

#[test]
fn artifact_intermediate_merge_uses_exact_source_sha_not_target_branch_collision() {
    let f = Fixture::new();
    git(&f.target, ["branch", "feature", "main"]);
    f.ok(&[
        "land",
        "--schema-version",
        "0.1",
        "apply",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--target",
        "staging",
        "--out",
        "out.json",
    ]);
    let log = fs::read_to_string(f.root.join("api.log")).unwrap();
    let line = log
        .lines()
        .find(|l| l.starts_with("POST /repos/upstream/service/merges "))
        .unwrap();
    let payload: Value = serde_json::from_str(line.split_once("HTTP/1.1 ").unwrap().1).unwrap();
    assert_eq!(
        payload["head"],
        git(&f.source, ["rev-parse", "feature"]).trim()
    );
    assert_eq!(payload["base"], "staging");
}

#[test]
fn local_update_gate_accepts_new_fork_head_without_relaxing_source_identity() {
    let f = Fixture::new();
    let project = read(&f.project);
    write(
        &f.root.join(format!(
            ".knit/projects/{}.project.json",
            project["id"].as_str().unwrap()
        )),
        &project,
    );
    let mut bundle = read(&f.bundle);
    bundle["projectId"] = project["id"].clone();
    write(&f.bundle, &bundle);
    f.ok(&["land", "plan", "--out", "gate.json"]);
    let mut plan = read(&f.root.join("gate.json"));
    plan["requiredExecutorVersion"] = json!("0.6");
    plan["requiredCapabilities"]
        .as_array_mut()
        .unwrap()
        .push(json!("landing-gates"));
    let steps = plan["steps"].as_array_mut().unwrap();
    steps.iter_mut().find(|s| s["type"] == "merge_pr").unwrap()["needs"] = json!(["bump"]);
    steps.push(json!({"id":"bump","type":"await_update","repoId":"service","instructions":"Push dependency update","paths":["Cargo.toml"],"effect":"read_only"}));
    write(&f.root.join("gate.json"), &plan);
    let paused: Value = serde_json::from_str(&f.ok(&[
        "land",
        "apply",
        "--plan",
        "gate.json",
        "--no-remote",
        "--keep-worktrees",
        "--json",
    ]))
    .unwrap();
    assert_eq!(paused["status"], "paused");
    fs::write(
        f.checkout.join("Cargo.toml"),
        "# synthetic dependency bump\n",
    )
    .unwrap();
    git(&f.checkout, ["add", "Cargo.toml"]);
    git(&f.checkout, ["commit", "-m", "Dependency bump"]);
    git(&f.checkout, ["push", "origin", "feature"]);
    let accepted = git(&f.checkout, ["rev-parse", "HEAD"]).trim().to_owned();
    *f.wrong_head.lock().unwrap() = true;
    let refused = f.cmd(&["land", "resume", "--no-remote"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("contradicts"));
    assert!(!fs::read_to_string(f.root.join("api.log"))
        .unwrap()
        .contains("PUT "));
    *f.wrong_head.lock().unwrap() = false;
    let run: Value = serde_json::from_str(&f.ok(&[
        "land",
        "resume",
        "--no-remote",
        "--keep-worktrees",
        "--json",
    ]))
    .unwrap();
    assert_eq!(run["status"], "succeeded");
    let gate = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "bump")
        .unwrap();
    assert_eq!(gate["output"]["revision"], accepted);
    assert_eq!(gate["output"]["sourceRepository"], "contributor/service");
    let log = fs::read_to_string(f.root.join("api.log")).unwrap();
    let merge = log.lines().find(|line| line.starts_with("PUT ")).unwrap();
    assert!(merge.contains(&format!("\"sha\":\"{accepted}\"")));
}

#[test]
fn landing_waits_for_maintainers_to_merge_a_review_this_account_cannot_merge() {
    let f = Fixture::new();
    f.maintainers_merge();
    f.ok(&["land", "plan", "--out", "plan.json"]);
    let plan = read(&f.root.join("plan.json"));
    let step = &plan["steps"][0];
    assert_eq!(step["mergedBy"], "upstream", "{plan}");
    assert_eq!(step["effect"], "read_only");
    assert_eq!(step["recovery"]["mode"], "none");
    assert_eq!(plan["requiredExecutorVersion"], "0.6");

    let apply = [
        "land",
        "apply",
        "--plan",
        "plan.json",
        "--no-remote",
        "--keep-worktrees",
    ];
    let output = f.cmd(&apply);
    let paused = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{paused}");
    assert!(paused.contains("\"paused\""), "{paused}");
    assert!(
        paused.contains("maintainers merge https://github.com/upstream/service/pull/7"),
        "{paused}"
    );
    let log = fs::read_to_string(f.root.join("api.log")).unwrap();
    assert!(!log.lines().any(|l| l.starts_with("PUT ")), "{log}");
    let run_path = f.run_path();
    assert_eq!(read(&run_path)["status"], "paused");

    let head = git(&f.source, ["rev-parse", "feature"]).trim().to_owned();
    git(
        &f.source,
        [
            "push",
            "--force",
            f.target.to_str().unwrap(),
            "feature:main",
        ],
    );
    *f.maintainer_merged.lock().unwrap() = true;
    f.ok(&["land", "resume", "--no-remote", "--keep-worktrees"]);
    let run = read(&run_path);
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["steps"][0]["output"]["revision"], head);
    assert_eq!(read(&f.bundle)["state"], "archived");
    let log = fs::read_to_string(f.root.join("api.log")).unwrap();
    assert!(!log.lines().any(|l| l.starts_with("PUT ")), "{log}");
}

const APPLY: [&str; 6] = [
    "land",
    "apply",
    "--plan",
    "plan.json",
    "--no-remote",
    "--keep-worktrees",
];

fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn landing_records_a_review_a_maintainer_rebased_before_merging_it() {
    let f = Fixture::new();
    f.maintainers_merge();
    let reviewed = git(&f.source, ["rev-parse", "feature"]).trim().to_owned();
    let merged = f.maintainer_rebases(None);
    assert_ne!(merged, reviewed);
    f.maintainer_merges();

    let check = f.ok(&["land", "check"]);
    assert!(check.contains("already landed"), "{check}");
    assert!(!check.contains("unavailable"), "{check}");

    f.ok(&["land", "plan", "--out", "plan.json"]);
    let output = f.cmd(&APPLY);
    let text = combined(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("the change is the same"), "{text}");
    let run = read(&f.run_path());
    assert_eq!(run["status"], "succeeded", "{run}");
    let receipt = &run["steps"][0]["output"];
    assert_eq!(receipt["reviewedHead"], reviewed, "{receipt}");
    assert_eq!(receipt["mergedHead"], merged, "{receipt}");
    assert_eq!(receipt["revision"], merged, "{receipt}");
    assert_eq!(receipt["patchMatches"], true, "{receipt}");
    assert_eq!(read(&f.bundle)["state"], "archived");
    let log = fs::read_to_string(f.root.join("api.log")).unwrap();
    assert!(!log.lines().any(|l| l.starts_with("PUT ")), "{log}");
}

#[test]
fn a_paused_landing_keeps_waiting_when_a_maintainer_rewrites_the_open_review() {
    let f = Fixture::new();
    f.maintainers_merge();
    f.ok(&["land", "plan", "--out", "plan.json"]);
    let reviewed = git(&f.source, ["rev-parse", "feature"]).trim().to_owned();
    let paused = f.cmd(&APPLY);
    assert!(paused.status.success(), "{}", combined(&paused));
    assert_eq!(read(&f.run_path())["status"], "paused");

    let merged = f.maintainer_rebases(None);
    let check = f.ok(&["land", "check"]);
    assert!(
        check.contains("awaiting maintainers (branch updated by a maintainer)"),
        "{check}"
    );
    let resume = ["land", "resume", "--no-remote", "--keep-worktrees"];
    let waiting = f.cmd(&resume);
    let text = combined(&waiting);
    assert!(waiting.status.success(), "{text}");
    assert!(text.contains("A maintainer updated its branch"), "{text}");
    assert_eq!(read(&f.run_path())["status"], "paused");

    f.maintainer_merges();
    f.ok(&resume);
    let run = read(&f.run_path());
    assert_eq!(run["status"], "succeeded", "{run}");
    let receipt = &run["steps"][0]["output"];
    assert_eq!(receipt["reviewedHead"], reviewed, "{receipt}");
    assert_eq!(receipt["mergedHead"], merged, "{receipt}");
    assert_eq!(receipt["patchMatches"], true, "{receipt}");
    assert_eq!(read(&f.bundle)["state"], "archived");
}

#[test]
fn a_review_merged_with_a_different_change_lands_with_a_note() {
    let f = Fixture::new();
    f.maintainers_merge();
    let merged = f.maintainer_rebases(Some("maintainer version\n"));
    f.maintainer_merges();
    f.ok(&["land", "plan", "--out", "plan.json"]);
    let output = f.cmd(&APPLY);
    let text = combined(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("merged a different change"), "{text}");
    let run = read(&f.run_path());
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["steps"][0]["output"]["mergedHead"], merged);
    assert_eq!(run["steps"][0]["output"]["patchMatches"], false);
}

#[test]
fn a_rewritten_review_is_still_refused_where_this_account_can_merge() {
    let f = Fixture::new();
    f.maintainer_rebases(None);
    let result = f.cmd(&["land", "check"]);
    assert!(
        combined(&result).contains("contradicts"),
        "{}",
        combined(&result)
    );
}

#[test]
fn a_rewritten_review_from_another_source_repository_is_refused() {
    let f = Fixture::new();
    f.maintainers_merge();
    f.maintainer_rebases(None);
    f.maintainer_merges();
    *f.wrong_head.lock().unwrap() = true;
    let result = f.cmd(&["land", "check"]);
    let text = combined(&result);
    assert!(text.contains("contradicts"), "{text}");
    assert!(text.contains("PR unavailable"), "{text}");
}
