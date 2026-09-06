// Target readiness currently uses Unix statvfs and du. Exercise the supported
// Linux/macOS flow here; platform-independent model/schema tests still run everywhere.
#![cfg(unix)]

mod common;
use common::*;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    target: PathBuf,
    api: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let source = root.join("source");
        let target = root.join("target");
        let api = root.join("api");
        let home = root.join("knit-home");
        let (_origin, repo, _) = init_remote_repo(&root, "backend");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&home).unwrap();
        knit(&source, ["init", "demo"]);
        knit(
            &source,
            ["project", "add", "backend", repo.to_str().unwrap()],
        );
        knit(&source, ["bundle", "travel"]);
        let url = spawn_fake_remote_push_api(&api);
        fs::write(home.join("config.json"),json!({"schemaVersion":"0.1","syncRemote":"hosted","remotes":{"hosted":{"url":url,"token":"test-token"}}}).to_string()).unwrap();
        let f = Self {
            root,
            source,
            target,
            api,
            home,
        };
        f.stage_export(&f.source);
        f
    }
    fn run(&self, cwd: &Path, args: &[&str], environment: &str) -> (String, String, bool) {
        knit_split_output(
            cwd,
            args,
            &[
                ("KNIT_HOME", self.home.to_str().unwrap()),
                ("KNIT_ENVIRONMENT_ID", environment),
                ("GIT_AUTHOR_NAME", "Test"),
                ("GIT_AUTHOR_EMAIL", "test@example.test"),
                ("GIT_COMMITTER_NAME", "Test"),
                ("GIT_COMMITTER_EMAIL", "test@example.test"),
            ],
        )
    }
    fn ok(&self, cwd: &Path, args: &[&str], env: &str) -> Value {
        let (o, e, s) = self.run(cwd, args, env);
        assert!(s, "{args:?}: {e}\n{o}");
        serde_json::from_str(&o).unwrap_or_else(|e| panic!("JSON parse: {e}: {o}"))
    }
    fn bundle(&self, cwd: &Path) -> Value {
        serde_json::from_str(
            &fs::read_to_string(cwd.join(".knit/bundles/travel.bundle.json")).unwrap(),
        )
        .unwrap()
    }
    fn stage_export(&self, cwd: &Path) {
        let payload = self.bundle(cwd);
        let project: Value = serde_json::from_str(
            &fs::read_to_string(cwd.join(".knit/projects/demo.project.json")).unwrap(),
        )
        .unwrap();
        let repos:Vec<_>=project["repos"].as_array().unwrap().iter().map(|r|json!({"localId":r["id"],"name":r["id"],"defaultBranch":r["baseBranch"],"remoteUrl":r["remote"],"visibility":"public","metadata":{}})).collect();
        fs::write(self.api.join("export.json"),json!({"data":{"project":{"id":"p-1","slug":"demo"},"knitProject":project,"repositories":repos,"bundles":[{"id":"rb-travel","slug":"travel","lifecycleState":"open","currentArtifact":{"artifactHash":"fakehash","payload":payload}}]}}).to_string()).unwrap();
    }
    fn accept(&self, cwd: &Path, env: &str) -> Value {
        self.ok(
            &self.root,
            &[
                "handoff",
                "in",
                "acme/demo",
                "travel",
                "--workspace",
                cwd.to_str().unwrap(),
                "--json",
            ],
            env,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn checkpoint_round_trip_and_idempotent_acceptance() {
    let f = Fixture::new();
    let source_tree = f.source.join(".knit/worktrees/travel/backend");
    fs::write(source_tree.join("wip.txt"), "laptop work\n").unwrap();
    let out = f.ok(
        &f.source,
        &["handoff", "out", "--to", "vps", "--json"],
        "laptop",
    );
    assert_eq!(out["checkpointCommitGroupIds"].as_array().unwrap().len(), 1);
    assert_eq!(git(&source_tree, ["status", "--porcelain"]).trim(), "");
    let (_, error, success) = f.run(&f.source, &["handoff", "out", "--json"], "laptop");
    assert!(!success);
    assert!(error.contains("already handed off"));
    f.stage_export(&f.source);
    let incoming = f.accept(&f.target, "vps");
    assert_eq!(incoming["bundleId"], "travel");
    let target_tree = f.target.join(".knit/worktrees/travel/backend");
    assert_eq!(
        fs::read_to_string(target_tree.join("wip.txt")).unwrap(),
        "laptop work\n"
    );
    f.stage_export(&f.target);
    f.accept(&f.target, "vps");
    assert_eq!(
        f.bundle(&f.target)["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["type"] == "handoff.in")
            .count(),
        1
    );
    fs::write(target_tree.join("return.txt"), "VPS work\n").unwrap();
    f.ok(
        &f.target,
        &["handoff", "out", "--to", "laptop", "--json"],
        "vps",
    );
    f.stage_export(&f.target);
    f.accept(&f.source, "laptop");
    assert_eq!(
        fs::read_to_string(source_tree.join("return.txt")).unwrap(),
        "VPS work\n"
    );
    let status = f.ok(&f.source, &["handoff", "status", "--json"], "laptop");
    assert_eq!(status["location"]["state"], "active");
    assert_eq!(status["location"]["origin"]["environmentId"], "laptop");
}

#[test]
fn force_never_overwrites_dirty_target() {
    let f = Fixture::new();
    f.ok(&f.source, &["handoff", "out", "--json"], "laptop");
    f.stage_export(&f.source);
    f.accept(&f.target, "vps");
    let file = f
        .target
        .join(".knit/worktrees/travel/backend/private-wip.txt");
    fs::write(&file, "keep me").unwrap();
    let (_, error, success) = f.run(
        &f.root,
        &[
            "handoff",
            "in",
            "demo",
            "travel",
            "--workspace",
            f.target.to_str().unwrap(),
            "--force",
            "--json",
        ],
        "vps",
    );
    assert!(!success);
    assert!(error.contains("blocking") || error.contains("uncommitted"));
    assert_eq!(fs::read_to_string(file).unwrap(), "keep me");
}

#[test]
fn failed_artifact_publication_resumes_without_duplicate_checkpoints() {
    let f = Fixture::new();
    fs::write(
        f.source.join(".knit/worktrees/travel/backend/wip.txt"),
        "keep\n",
    )
    .unwrap();
    fs::write(f.api.join("enforce-fast-forward"), "").unwrap();
    let (_, _, success) = f.run(
        &f.source,
        &["handoff", "out", "--to", "vps", "--json"],
        "laptop",
    );
    assert!(!success);
    assert_eq!(
        f.bundle(&f.source)["commitGroups"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fs::remove_file(f.api.join("enforce-fast-forward")).unwrap();
    f.ok(
        &f.source,
        &["handoff", "out", "--to", "vps", "--json"],
        "laptop",
    );
    let b = f.bundle(&f.source);
    assert_eq!(b["commitGroups"].as_array().unwrap().len(), 1);
    assert_eq!(
        b["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["type"] == "handoff.out")
            .count(),
        1
    );
}

#[test]
fn probe_distinguishes_required_optional_and_runtime_tools() {
    let f = Fixture::new();
    let path = f.api.join("export.json");
    let mut export: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    export["data"]["knitProject"]["requirements"] = json!({"tools":[{"name":"knit-nonexistent-required-tool"},{"name":"knit-nonexistent-optional-tool","optional":true},{"name":"knit-nonexistent-runtime-tool","for":"runtime"}]});
    fs::write(path, export.to_string()).unwrap();
    let (o, _, s) = f.run(
        &f.root,
        &[
            "handoff",
            "probe",
            "demo",
            "travel",
            "--workspace",
            f.target.to_str().unwrap(),
            "--json",
        ],
        "vps",
    );
    assert!(!s);
    let r: Value = serde_json::from_str(&o).unwrap();
    assert_eq!(r["verdict"], "fail");
    let checks = r["checks"].as_array().unwrap();
    assert_eq!(
        checks
            .iter()
            .find(|c| c["id"] == "tool:knit-nonexistent-required-tool")
            .unwrap()["status"],
        "fail"
    );
    assert_eq!(
        checks
            .iter()
            .find(|c| c["id"] == "tool:knit-nonexistent-optional-tool")
            .unwrap()["status"],
        "warn"
    );
    assert_eq!(
        checks
            .iter()
            .find(|c| c["id"] == "tool:knit-nonexistent-runtime-tool")
            .unwrap()["status"],
        "warn"
    );
}

#[test]
fn partial_multi_repo_commit_records_success_before_retry() {
    let f = Fixture::new();
    let (_, other, _) = init_remote_repo(&f.root, "frontend");
    knit(
        &f.source,
        ["project", "add", "frontend", other.to_str().unwrap()],
    );
    knit(&f.source, ["bundle", "add", "frontend"]);
    for repo in ["backend", "frontend"] {
        fs::write(
            f.source
                .join(format!(".knit/worktrees/travel/{repo}/wip.txt")),
            repo,
        )
        .unwrap();
    }
    let hook = other.join(".git/hooks/pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (_, e, s) = f.run(&f.source, &["handoff", "out", "--json"], "laptop");
    assert!(!s, "{e}");
    assert_eq!(
        f.bundle(&f.source)["commitGroups"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fs::remove_file(hook).unwrap();
    f.ok(&f.source, &["handoff", "out", "--json"], "laptop");
    assert_eq!(
        f.bundle(&f.source)["commitGroups"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn existing_workspace_clones_missing_bundle_repositories() {
    let f = Fixture::new();
    let (_, frontend, _) = init_remote_repo(&f.root, "frontend");
    knit(
        &f.source,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );
    knit(&f.source, ["bundle", "add", "frontend"]);
    let source_tree = f.source.join(".knit/worktrees/travel/frontend");
    fs::write(source_tree.join("handoff.txt"), "frontend checkpoint").unwrap();
    f.ok(&f.source, &["handoff", "out", "--json"], "laptop");
    f.stage_export(&f.source);

    fs::create_dir_all(&f.target).unwrap();
    knit(&f.target, ["init", "demo"]);
    fs::write(f.target.join("workspace-notes.txt"), "keep workspace notes").unwrap();
    let accepted = f.accept(&f.target, "vps");
    assert_eq!(accepted["bundleSlug"], "travel");
    for repo in ["backend", "frontend"] {
        assert!(f.target.join(repo).join(".git").exists());
        assert!(f
            .target
            .join(format!(".knit/worktrees/travel/{repo}/.git"))
            .exists());
    }
    assert_eq!(
        fs::read_to_string(f.target.join(".knit/worktrees/travel/frontend/handoff.txt")).unwrap(),
        "frontend checkpoint"
    );
    assert_eq!(
        fs::read_to_string(f.target.join("workspace-notes.txt")).unwrap(),
        "keep workspace notes"
    );
    f.accept(&f.target, "vps");
    assert_eq!(f.bundle(&f.target)["repos"].as_array().unwrap().len(), 2);
}

#[test]
fn failed_acceptance_publication_reuses_the_saved_incoming_node() {
    let f = Fixture::new();
    let outgoing = f.ok(&f.source, &["handoff", "out", "--json"], "laptop");
    f.stage_export(&f.source);
    fs::write(f.api.join("reject-node-type"), "handoff.in").unwrap();
    let (_, error, success) = f.run(
        &f.root,
        &[
            "handoff",
            "in",
            "acme/demo",
            "travel",
            "--workspace",
            f.target.to_str().unwrap(),
            "--json",
        ],
        "vps",
    );
    assert!(!success);
    assert!(error.contains("Acceptance saved locally"), "{error}");
    let before = f.bundle(&f.target);
    let incoming: Vec<_> = before["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "handoff.in")
        .cloned()
        .collect();
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0]["handoff"]["id"], outgoing["handoffId"]);
    fs::remove_file(f.api.join("reject-node-type")).unwrap();
    // Keep the remote export at the outgoing checkpoint: acceptance never arrived.
    let resumed = f.accept(&f.target, "vps");
    assert_eq!(resumed["handoffId"], outgoing["handoffId"]);
    let after = f.bundle(&f.target);
    let retried: Vec<_> = after["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "handoff.in")
        .cloned()
        .collect();
    assert_eq!(retried, incoming);
}

#[test]
fn failure_after_outgoing_node_is_saved_republishes_the_same_handoff() {
    let f = Fixture::new();
    fs::write(
        f.source.join(".knit/worktrees/travel/backend/wip.txt"),
        "checkpoint once",
    )
    .unwrap();
    fs::write(f.api.join("reject-node-type"), "handoff.out").unwrap();
    let (_, error, success) = f.run(
        &f.source,
        &["handoff", "out", "--to", "vps", "--json"],
        "laptop",
    );
    assert!(!success);
    assert!(error.contains("Handoff saved locally"), "{error}");
    let before = f.bundle(&f.source);
    let outgoing: Vec<_> = before["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "handoff.out")
        .cloned()
        .collect();
    assert_eq!(outgoing.len(), 1);
    assert_eq!(before["commitGroups"].as_array().unwrap().len(), 1);
    let attempted: Vec<Value> = fs::read_to_string(f.api.join("artifact-travel.bodies"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(attempted.iter().any(|body| !body["payload"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["type"] == "handoff.out")));
    assert!(attempted.iter().any(|body| body["payload"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["type"] == "handoff.out")));
    fs::remove_file(f.api.join("reject-node-type")).unwrap();
    let resumed = f.ok(
        &f.source,
        &["handoff", "out", "--to", "vps", "--json"],
        "laptop",
    );
    assert_eq!(resumed["handoffId"], outgoing[0]["handoff"]["id"]);
    let after = f.bundle(&f.source);
    assert_eq!(after["commitGroups"], before["commitGroups"]);
    let retried: Vec<_> = after["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["type"] == "handoff.out")
        .cloned()
        .collect();
    assert_eq!(retried, outgoing);
}

#[test]
fn diverged_ledgers_are_preserved_and_force_still_requires_merge() {
    let f = Fixture::new();
    f.ok(&f.source, &["handoff", "out", "--json"], "laptop");
    f.stage_export(&f.source);
    f.accept(&f.target, "vps");
    let mut remote = f.bundle(&f.source);
    remote["nodes"].as_array_mut().unwrap().push(json!({"id":"source-only", "type":"checkpoint", "message":"source event", "createdAt":"2026-09-05T12:00:00Z"}));
    remote["headNodeId"] = json!("source-only");
    fs::write(
        f.source.join(".knit/bundles/travel.bundle.json"),
        remote.to_string(),
    )
    .unwrap();
    f.stage_export(&f.source);
    let path = f.target.join(".knit/bundles/travel.bundle.json");
    let before = fs::read(&path).unwrap();
    let (_, error, success) = f.run(
        &f.root,
        &[
            "handoff",
            "in",
            "acme/demo",
            "travel",
            "--workspace",
            f.target.to_str().unwrap(),
            "--force",
            "--json",
        ],
        "vps",
    );
    assert!(!success);
    assert!(error.contains("pull --merge"), "{error}");
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn diverged_feature_branches_preserve_local_commits_and_need_git_resolution() {
    let f = Fixture::new();
    f.ok(&f.source, &["handoff", "out", "--json"], "laptop");
    f.stage_export(&f.source);
    f.accept(&f.target, "vps");
    f.stage_export(&f.target);
    let source_tree = f.source.join(".knit/worktrees/travel/backend");
    let target_tree = f.target.join(".knit/worktrees/travel/backend");
    for (tree, file) in [
        (&source_tree, "source-only.txt"),
        (&target_tree, "target-only.txt"),
    ] {
        configure_git_user(tree);
        fs::write(tree.join(file), file).unwrap();
        git(tree, ["add", file]);
        git(tree, ["commit", "-m", file]);
    }
    git(&source_tree, ["push", "origin", "knit/travel"]);
    let before = git(&target_tree, ["rev-parse", "HEAD"]);
    let (_, error, success) = f.run(
        &f.root,
        &[
            "handoff",
            "in",
            "acme/demo",
            "travel",
            "--workspace",
            f.target.to_str().unwrap(),
            "--force",
            "--json",
        ],
        "vps",
    );
    assert!(!success);
    assert!(
        error.contains("fast-forward") || error.contains("diverg"),
        "{error}"
    );
    assert_eq!(git(&target_tree, ["rev-parse", "HEAD"]), before);
    assert_eq!(
        fs::read_to_string(target_tree.join("target-only.txt")).unwrap(),
        "target-only.txt"
    );
    assert!(!target_tree.join("source-only.txt").exists());
}

#[test]
fn retry_does_not_add_new_edits_to_an_already_recorded_snapshot() {
    let f = Fixture::new();
    let tree = f.source.join(".knit/worktrees/travel/backend");
    fs::write(tree.join("wip.txt"), "original checkpoint\n").unwrap();
    fs::write(f.api.join("reject-node-type"), "handoff.out").unwrap();
    let (_, _, success) = f.run(&f.source, &["handoff", "out", "--json"], "laptop");
    assert!(!success);
    let before = f.bundle(&f.source);
    fs::write(tree.join("new.txt"), "newer work\n").unwrap();
    fs::remove_file(f.api.join("reject-node-type")).unwrap();
    let (_, error, success) = f.run(&f.source, &["handoff", "out", "--json"], "laptop");
    assert!(!success);
    assert!(error.contains("immutable"), "{error}");
    assert_eq!(f.bundle(&f.source), before);
    assert_eq!(
        fs::read_to_string(tree.join("new.txt")).unwrap(),
        "newer work\n"
    );
}

#[test]
fn handoff_preserves_project_membership_without_reshaping_the_remote() {
    let f = Fixture::new();
    let (_, spare, _) = init_remote_repo(&f.root, "spare");
    knit(
        &f.source,
        [
            "project",
            "add",
            "spare",
            spare.to_str().unwrap(),
            "--observe",
        ],
    );
    f.ok(&f.source, &["handoff", "out", "--json"], "laptop");
    f.stage_export(&f.source);
    fs::remove_file(f.api.join("project-shape-writes.jsonl")).unwrap();
    f.accept(&f.target, "vps");
    let project: Value = serde_json::from_str(
        &fs::read_to_string(f.target.join(".knit/projects/demo.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(project["repos"].as_array().unwrap().len(), 2);
    assert!(
        !f.target.join("spare").exists(),
        "unselected repo must not be cloned"
    );
    assert!(!f.api.join("project-shape-writes.jsonl").exists());
}
