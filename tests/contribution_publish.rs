mod common;
use common::*;
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

const SOURCE: &str = "https://github.com/contributor/widget.git";
const TARGET: &str = "https://github.com/upstream/widget.git";

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    checkout: PathBuf,
    upstream: PathBuf,
    fork: PathBuf,
    bin: PathBuf,
    api: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let (upstream, local, _) = init_remote_repo(&root, "widget");
        let fork = root.join("fork.git");
        git(
            &root,
            [
                "clone",
                "--bare",
                upstream.to_str().unwrap(),
                fork.to_str().unwrap(),
            ],
        );
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        knit(&workspace, ["bundle", "contribution"]);
        knit(&workspace, ["bundle", "add", local.to_str().unwrap()]);
        let checkout = workspace.join(".knit/worktrees/contribution/widget");
        append_line(&checkout.join("app.txt"), "contribution one");
        knit(&workspace, ["commit", "--all", "-m", "Contribution one"]);
        git(&local, ["remote", "set-url", "origin", TARGET]);
        git(&local, ["remote", "set-url", "--push", "origin", SOURCE]);
        for (url, path) in [(SOURCE, &fork), (TARGET, &upstream)] {
            git(
                &local,
                ["config", &format!("url.{}.insteadOf", path.display()), url],
            );
        }
        let api = root.join("api");
        let bin = root.join("bin");
        fs::create_dir_all(&api).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(api.join("settings.json"), serde_json::to_vec(&json!({
            "source": "contributor/widget", "target": "upstream/widget", "fork": fork, "upstream": upstream
        })).unwrap()).unwrap();
        write_fake_python_cli(&bin, "gh", include_str!("fixtures/contribution_gh.py"));
        Self {
            root,
            workspace,
            checkout,
            upstream,
            fork,
            bin,
            api,
        }
    }
    fn run(&self, args: &[&str]) -> String {
        knit_with_fake_gh_env(
            &self.workspace,
            args,
            &self.bin,
            &self.api,
            &[("KNIT_GITHUB_API_TRANSPORT", "gh")],
        )
    }
    fn fail(&self, args: &[&str]) -> String {
        knit_fails_with_fake_gh_env(
            &self.workspace,
            args,
            &self.bin,
            &self.api,
            &[("KNIT_GITHUB_API_TRANSPORT", "gh")],
        )
    }
    fn create(&self) {
        self.run(&[
            "publish",
            "create",
            "--source-remote",
            "origin",
            "--target-remote",
            "origin",
            "--no-remote",
        ]);
    }
    fn clear_push_receipts(&self) {
        let refs = git(
            &self.checkout,
            [
                "for-each-ref",
                "--format=%(refname)",
                "refs/knit/contributions/",
            ],
        );
        for reference in refs.lines() {
            git(&self.checkout, ["update-ref", "-d", reference]);
        }
    }
    fn foreign_fork_commit(&self) -> String {
        // Create an object without ever recording it in the bundle or the
        // feature branch's reflog. The base itself is a known, allowed tip.
        configure_git_user(&self.fork);
        git(
            &self.fork,
            [
                "commit-tree",
                "main^{tree}",
                "-p",
                "main",
                "-m",
                "Foreign fork contribution",
            ],
        )
    }
    fn artifact(&self) -> PathBuf {
        self.workspace
            .join(".knit/bundles/contribution.bundle.json")
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.api.join("calls")).unwrap_or_default()
    }
    fn bundle(&self) -> Value {
        serde_json::from_slice(&fs::read(self.artifact()).unwrap()).unwrap()
    }
    fn save(&self, v: &Value) {
        fs::write(self.artifact(), serde_json::to_vec(v).unwrap()).unwrap();
    }
}

#[test]
fn split_pushurl_create_update_retarget_and_portable_sync() {
    let f = Fixture::new();
    f.create();
    let first = git(&f.fork, ["rev-parse", "knit/contribution"]);
    assert_eq!(first, git(&f.checkout, ["rev-parse", "HEAD"]));
    let bundle = f.bundle();
    assert_eq!(bundle["repos"][0]["sourceRemote"], SOURCE);
    assert_eq!(bundle["repos"][0]["targetRemote"], TARGET);
    assert_eq!(
        bundle["publications"][0]["url"],
        "https://github.com/upstream/widget/pull/7"
    );
    assert!(!fs::read_to_string(f.api.join("create.json"))
        .unwrap()
        .contains("\"draft\": true"));
    append_line(&f.checkout.join("app.txt"), "contribution two");
    knit(&f.workspace, ["commit", "--all", "-m", "Contribution two"]);
    f.run(&["publish", "create", "--no-remote"]);
    assert_ne!(first, git(&f.fork, ["rev-parse", "knit/contribution"]));
    assert_eq!(
        f.calls()
            .matches("POST repos/upstream/widget/pulls\n")
            .count(),
        1
    );
    git(&f.upstream, ["branch", "stable", "main"]);
    f.run(&["publish", "create", "--target", "stable", "--no-remote"]);
    assert_eq!(f.bundle()["publications"][0]["baseBranch"], "stable");
    let output = f.root.join("portable.json");
    f.run(&[
        "publish",
        "sync",
        "--from-artifact",
        f.artifact().to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
    ]);
    let portable: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(portable["repos"][0]["sourceRemote"], SOURCE);
    assert_eq!(portable["repos"][0]["targetRemote"], TARGET);
    assert!(!f.calls().contains("repos/contributor/widget/pulls"));
}

#[test]
fn sync_records_what_the_maintainers_still_have_to_do() {
    let f = Fixture::new();
    f.create();
    fs::write(
        f.api.join("gates.json"),
        json!({
            "reviewDecision": "REVIEW_REQUIRED",
            "rules": [
                {"type": "pull_request", "parameters": {"required_approving_review_count": 1}},
                {"type": "required_status_checks", "parameters": {"required_status_checks": [{"context": "build"}]}},
                {"type": "required_signatures"}
            ],
            "awaiting": ["CI"],
            "prChecks": [{"name": "lint", "state": "FAILURE", "bucket": "fail", "link": "https://example.test/runs/1"}],
            "commits": [{"commit": {"verification": {"verified": false}}}],
            "push": false
        })
        .to_string(),
    )
    .unwrap();
    let gated = f.root.join("gated.json");
    let output = f.run(&[
        "publish",
        "sync",
        "--from-artifact",
        f.artifact().to_str().unwrap(),
        "--out",
        gated.to_str().unwrap(),
    ]);
    assert!(
        output.contains(
            "waiting on maintainers (review, CI approval, checks, signed commits, merge)"
        ),
        "{output}"
    );
    let read =
        |path: &PathBuf| -> Value { serde_json::from_slice(&fs::read(path).unwrap()).unwrap() };
    let first = read(&gated);
    let gates = &first["publications"][0]["gates"];
    let seen: Vec<(&str, &str, &str)> = gates["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|gate| {
            (
                gate["kind"].as_str().unwrap(),
                gate["state"].as_str().unwrap(),
                gate["actor"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("review", "pending", "maintainers"),
            ("ci_approval", "pending", "maintainers"),
            ("checks", "pending", "maintainers"),
            ("signatures", "pending", "maintainers"),
            ("merge_permission", "pending", "maintainers"),
        ]
    );
    assert_eq!(
        gates["items"][2]["summary"],
        "required checks not reported yet: build"
    );
    assert_eq!(
        first["publications"][0]["checks"]["items"],
        json!([{"name": "lint", "state": "failure", "url": "https://example.test/runs/1"}])
    );

    let again = f.root.join("again.json");
    f.run(&[
        "publish",
        "sync",
        "--from-artifact",
        gated.to_str().unwrap(),
        "--out",
        again.to_str().unwrap(),
    ]);
    assert_eq!(
        read(&again)["publications"][0]["gates"],
        first["publications"][0]["gates"],
        "unchanged gates keep their timestamp"
    );
    assert_eq!(
        read(&again)["publications"][0]["checks"],
        first["publications"][0]["checks"]
    );
}

#[test]
fn a_bundle_whose_reviews_all_merged_closes_once_its_plan_has_nothing_left() {
    let f = Fixture::new();
    f.create();
    fs::write(
        f.api.join("override.json"),
        json!({"state": "closed", "merged": true, "merged_at": "2026-10-09T00:00:00Z"}).to_string(),
    )
    .unwrap();
    let plan = f.root.join("plan.json");
    fs::write(
        &plan,
        json!({"steps": [
            {"id": "merge-widget", "type": "merge_pr"},
            {"id": "deploy-widget", "type": "deploy"}
        ]})
        .to_string(),
    )
    .unwrap();
    let read =
        |path: &PathBuf| -> Value { serde_json::from_slice(&fs::read(path).unwrap()).unwrap() };
    let kept = f.root.join("kept.json");
    let output = f.run(&[
        "publish",
        "sync",
        "--from-artifact",
        f.artifact().to_str().unwrap(),
        "--out",
        kept.to_str().unwrap(),
        "--plan",
        plan.to_str().unwrap(),
    ]);
    assert!(output.contains("still has deploy-widget"), "{output}");
    assert_eq!(read(&kept)["publications"][0]["state"], "MERGED");
    assert_ne!(read(&kept)["state"], "archived");

    let closed = f.root.join("closed.json");
    f.run(&[
        "publish",
        "sync",
        "--from-artifact",
        f.artifact().to_str().unwrap(),
        "--out",
        closed.to_str().unwrap(),
    ]);
    assert_eq!(read(&closed)["state"], "archived");

    let output = f.run(&["publish", "sync"]);
    assert!(output.contains("Closed"), "{output}");
    assert_eq!(f.bundle()["state"], "archived");
}

#[test]
fn land_closes_a_bundle_the_host_already_merged() {
    let f = Fixture::new();
    f.create();
    fs::write(
        f.api.join("override.json"),
        json!({"state": "closed", "merged": true, "merged_at": "2026-10-09T00:00:00Z"}).to_string(),
    )
    .unwrap();
    let output = f.run(&["land"]);
    assert!(output.contains("merged on the host"), "{output}");
    assert!(output.contains("Closed"), "{output}");
    assert_eq!(f.bundle()["state"], "archived");
}

#[test]
fn wrong_same_number_url_and_wrong_review_identity_never_mutate() {
    let f = Fixture::new();
    f.create();
    let original = f.bundle();
    let mut wrong = original.clone();
    wrong["publications"][0]["url"] = json!("https://github.com/contributor/widget/pull/7");
    f.save(&wrong);
    fs::write(f.api.join("calls"), "").unwrap();
    assert!(f
        .fail(&["publish", "create", "--no-remote"])
        .contains("contradicts targetRemote"));
    assert!(f.calls().is_empty());
    f.save(&original);
    for (field, value) in [
        ("head.repo.full_name", "other/widget"),
        ("head.ref", "other-branch"),
        ("head.sha", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ("base.repo.full_name", "other/widget"),
        ("base.ref", "other-base"),
    ] {
        fs::write(
            f.api.join("override.json"),
            serde_json::to_vec(&json!({field: value})).unwrap(),
        )
        .unwrap();
        fs::write(f.api.join("calls"), "").unwrap();
        let error = f.fail(&[
            "publish",
            "sync",
            "--from-artifact",
            f.artifact().to_str().unwrap(),
        ]);
        assert!(error.contains("contradicts the contribution"), "{error}");
        assert!(!f.calls().contains("PATCH ") && !f.calls().contains("POST "));
    }
}

#[test]
fn unsupported_forge_and_null_identity_fail_before_effects() {
    let f = Fixture::new();
    let mut bundle = f.bundle();
    bundle["repos"][0]["sourceRemote"] = json!("https://gitlab.com/contributor/widget.git");
    bundle["repos"][0]["targetRemote"] = json!("https://gitlab.com/upstream/widget.git");
    f.save(&bundle);
    assert!(f
        .fail(&["publish", "create", "--no-remote"])
        .contains("github.com only"));
    assert!(f.calls().is_empty());
    bundle["repos"][0]["targetRemote"] = Value::Null;
    f.save(&bundle);
    assert!(f.fail(&["publish", "sync"]).contains("invalid type"));
    assert!(f.calls().is_empty());
}

#[test]
fn lifecycle_reads_source_feature_target_base_and_deletes_only_source() {
    let f = Fixture::new();
    f.create();
    // A same-named upstream feature branch is deliberately unrelated.
    git(&f.upstream, ["branch", "knit/contribution", "main"]);
    let upstream_trap = git(&f.upstream, ["rev-parse", "knit/contribution"]);
    knit(&f.workspace, ["fetch", "--mode", "git"]);
    // Restore from a portable bundle with no local feature branch.
    git(
        &f.workspace.join("../widget"),
        ["worktree", "remove", f.checkout.to_str().unwrap()],
    );
    git(
        &f.workspace.join("../widget"),
        ["branch", "-D", "knit/contribution"],
    );
    knit(&f.workspace, ["bundle", "worktree"]);
    assert_eq!(
        git(&f.checkout, ["rev-parse", "HEAD"]),
        git(&f.fork, ["rev-parse", "knit/contribution"])
    );
    assert_ne!(git(&f.checkout, ["rev-parse", "HEAD"]), upstream_trap);
    knit(&f.workspace, ["pull", "--feature", "--no-remote"]);
    knit(
        &f.workspace,
        [
            "bundle",
            "delete",
            "contribution",
            "--force",
            "--worktrees",
            "--branches",
            "--force-branches",
            "--remote-branches",
        ],
    );
    assert_eq!(
        git(&f.upstream, ["rev-parse", "knit/contribution"]),
        upstream_trap
    );
    assert!(git(&f.fork, ["branch", "--list", "knit/contribution"]).is_empty());
}

#[test]
fn artifact_creation_and_same_owner_fork_select_full_head_repository() {
    let f = Fixture::new();
    let source = "https://github.com/upstream/widget-fork.git";
    git(
        &f.checkout,
        [
            "config",
            &format!("url.{}.insteadOf", f.fork.display()),
            source,
        ],
    );
    git(
        &f.checkout,
        ["remote", "set-url", "--push", "origin", source],
    );
    let mut settings: Value =
        serde_json::from_slice(&fs::read(f.api.join("settings.json")).unwrap()).unwrap();
    settings["source"] = json!("upstream/widget-fork");
    fs::write(
        f.api.join("settings.json"),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let mut bundle = f.bundle();
    bundle["repos"][0]["sourceRemote"] = json!(source);
    bundle["repos"][0]["targetRemote"] = json!(TARGET);
    bundle["repos"][0]["path"] = json!(f.root.join("unavailable/checkout"));
    bundle["repos"][0]["worktreePath"] = Value::Null;
    f.save(&bundle);
    git(&f.checkout, ["push", "origin", "knit/contribution"]);
    let output = f.root.join("created.json");
    f.run(&[
        "publish",
        "create",
        "--from-artifact",
        f.artifact().to_str().unwrap(),
        "--no-push",
        "--out",
        output.to_str().unwrap(),
    ]);
    let payload: Value =
        serde_json::from_slice(&fs::read(f.api.join("create.json")).unwrap()).unwrap();
    assert_eq!(payload["head"], "upstream:knit/contribution");
    assert_eq!(payload["head_repo"], "widget-fork");
    let result: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(result["repos"][0]["sourceRemote"], source);
}

#[test]
fn land_update_continues_publication_after_target_base_moves() {
    let f = Fixture::new();
    f.create();
    let collaborator = f.root.join("widget-collaborator");
    fs::write(collaborator.join("base.txt"), "base movement\n").unwrap();
    git(&collaborator, ["add", "base.txt"]);
    git(&collaborator, ["commit", "-m", "Base movement"]);
    git(&collaborator, ["push", "origin", "main"]);
    let target_tip = git(&f.upstream, ["rev-parse", "main"]);
    f.run(&["land", "update", "--push"]);
    assert_eq!(
        git(&f.checkout, ["rev-parse", "HEAD"]),
        git(&f.fork, ["rev-parse", "knit/contribution"])
    );
    assert_eq!(
        git(&f.checkout, ["merge-base", "HEAD", target_tip.trim()]),
        target_tip
    );
    f.run(&["publish", "create", "--no-remote"]);
    assert_eq!(
        f.calls()
            .matches("POST repos/upstream/widget/pulls\n")
            .count(),
        1
    );
}

#[test]
fn source_and_target_api_credentials_are_resolved_independently() {
    check_credentials(false);
}

#[test]
fn unlisted_source_uses_host_default_without_inheriting_upstream_assignment() {
    check_credentials(true);
}

fn check_credentials(unlisted_source: bool) {
    let f = Fixture::new();
    f.create();
    knit(&f.workspace, ["init", "auth-test"]);
    let project_path = f.workspace.join(".knit/projects/auth-test.project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    project["repos"] = json!([
        {"id":"source", "path":f.checkout, "remote":SOURCE, "baseBranch":"main"},
        {"id":"target", "path":f.checkout, "remote":TARGET, "baseBranch":"main"}
    ]);
    if unlisted_source {
        project["repos"].as_array_mut().unwrap().remove(0);
    }
    fs::write(&project_path, serde_json::to_vec(&project).unwrap()).unwrap();
    let home = f.root.join("credentials");
    fs::create_dir(&home).unwrap();
    let key = fs::canonicalize(&project_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut registry = json!({
        "credentials": {
            "source": {"provider":"github", "host":"github.com", "tokenEnv":"TEST_SOURCE_TOKEN"},
            "target": {"provider":"github", "host":"github.com", "tokenEnv":"TEST_TARGET_TOKEN"}
        },
        "scopedCredentials":["source","target"],
        "projects": {key.clone(): {"source":"source", "target":"target"}}
    });
    if unlisted_source {
        registry["projects"][&key]
            .as_object_mut()
            .unwrap()
            .remove("source");
        registry["scopedCredentials"] = json!(["target"]);
        registry["defaults"] = json!({"github.com":"source"});
    }
    fs::write(
        home.join("forge-auth.json"),
        serde_json::to_vec(&registry).unwrap(),
    )
    .unwrap();
    fs::write(f.api.join("require-credentials"), "").unwrap();
    knit_with_fake_gh_env(
        &f.workspace,
        [
            "publish",
            "create",
            "--from-artifact",
            f.artifact().to_str().unwrap(),
            "--no-push",
            "--out",
            f.root.join("authed.json").to_str().unwrap(),
        ],
        &f.bin,
        &f.api,
        &[
            ("KNIT_HOME", home.to_str().unwrap()),
            ("KNIT_GITHUB_API_TRANSPORT", "gh"),
            ("TEST_SOURCE_TOKEN", "synthetic-source"),
            ("TEST_TARGET_TOKEN", "synthetic-target"),
        ],
    );
}

#[test]
fn explicit_same_repository_fields_preserve_ordinary_publication() {
    let f = Fixture::new();
    git(
        &f.checkout,
        ["remote", "set-url", "--push", "origin", TARGET],
    );
    let mut settings: Value =
        serde_json::from_slice(&fs::read(f.api.join("settings.json")).unwrap()).unwrap();
    settings["source"] = json!("upstream/widget");
    settings["fork"] = json!(f.upstream);
    fs::write(
        f.api.join("settings.json"),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let mut bundle = f.bundle();
    bundle["repos"][0]["headSha"] = Value::Null;
    f.save(&bundle);
    f.create();
    assert_eq!(f.bundle()["repos"][0]["sourceRemote"], TARGET);
    assert_eq!(f.bundle()["repos"][0]["targetRemote"], TARGET);
    f.run(&["publish", "sync"]);
    assert_eq!(
        f.calls()
            .matches("POST repos/upstream/widget/pulls\n")
            .count(),
        1
    );
}

#[test]
fn legacy_and_explicit_same_repo_contracts_roundtrip_without_fork_requirements() {
    use knit::contribution;
    // Same-repository fields arrive from hosted preparation on every supported forge.
    for remote in [
        TARGET,
        "https://gitlab.com/team/widget.git",
        "https://codeberg.org/team/widget.git",
        "https://bitbucket.org/team/widget.git",
    ] {
        let mut value =
            json!({"id":"widget", "path":"/portable", "remote":remote, "baseBranch":"main"});
        let legacy: knit::model::RepoEntry = serde_json::from_value(value.clone()).unwrap();
        assert!(contribution::identity(&legacy, "main").unwrap().is_none());
        assert!(serde_json::to_value(&legacy)
            .unwrap()
            .get("sourceRemote")
            .is_none());
        value["sourceRemote"] = json!(remote);
        value["targetRemote"] = json!(remote);
        let prepared: knit::model::RepoEntry = serde_json::from_value(value).unwrap();
        assert!(contribution::identity(&prepared, "main").unwrap().is_none());
        assert_eq!(
            serde_json::to_value(prepared).unwrap()["targetRemote"],
            remote
        );
    }
}

#[test]
fn explicit_target_supplies_blank_saved_base_in_artifact_create_and_sync() {
    let f = Fixture::new();
    f.create();
    git(&f.upstream, ["branch", "stable", "main"]);
    let mut bundle = f.bundle();
    bundle["repos"][0]["baseBranch"] = json!("");
    bundle["publications"] = json!([]);
    f.save(&bundle);
    fs::remove_file(f.api.join("pr.json")).unwrap();
    let output = f.root.join("blank-base.json");
    f.run(&[
        "publish",
        "create",
        "--from-artifact",
        f.artifact().to_str().unwrap(),
        "--target",
        "stable",
        "--no-push",
        "--out",
        output.to_str().unwrap(),
    ]);
    let result: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(result["repos"][0]["baseBranch"], "");
    assert_eq!(result["publications"][0]["baseBranch"], "stable");
    f.run(&[
        "publish",
        "sync",
        "--from-artifact",
        output.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
    ]);
}

#[test]
fn stale_recorded_base_adopts_only_intended_live_base() {
    let f = Fixture::new();
    f.create();
    git(&f.upstream, ["branch", "stable", "main"]);
    let mut pr: Value = serde_json::from_slice(&fs::read(f.api.join("pr.json")).unwrap()).unwrap();
    pr["base"]["ref"] = json!("stable");
    fs::write(f.api.join("pr.json"), serde_json::to_vec(&pr).unwrap()).unwrap();
    fs::write(f.api.join("calls"), "").unwrap();
    f.run(&[
        "publish",
        "create",
        "--target",
        "stable",
        "--no-sync",
        "--no-remote",
    ]);
    assert_eq!(f.bundle()["publications"][0]["baseBranch"], "stable");
    assert!(!f.calls().contains("POST ") && !f.calls().contains("PATCH "));
    pr["base"]["ref"] = json!("unrelated");
    fs::write(f.api.join("pr.json"), serde_json::to_vec(&pr).unwrap()).unwrap();
    fs::write(f.api.join("calls"), "").unwrap();
    assert!(f
        .fail(&["publish", "create", "--target", "stable", "--no-remote"])
        .contains("contradicts the contribution"));
    assert!(!f.calls().contains("POST ") && !f.calls().contains("PATCH "));
}

#[test]
fn git_pushurl_is_discovered_without_creating_or_rewriting_remotes() {
    let f = Fixture::new();
    let before = git(&f.checkout, ["config", "--get-regexp", "^remote\\."]);
    f.run(&["publish", "create", "--no-remote"]);
    assert_eq!(f.bundle()["repos"][0]["sourceRemote"], SOURCE);
    assert_eq!(f.bundle()["repos"][0]["targetRemote"], TARGET);
    assert_eq!(
        git(&f.checkout, ["config", "--get-regexp", "^remote\\."]),
        before
    );
}

// Run provider calls in a child so native transport environment is isolated.
#[test]
fn native_merge_provider_child() {
    let Ok(artifact) = std::env::var("CONTRIBUTION_MERGE_ARTIFACT") else {
        return;
    };
    let bundle: knit::model::ChangeGroup =
        serde_json::from_slice(&fs::read(artifact).unwrap()).unwrap();
    let forge = knit::providers::by_id("github").unwrap();
    let target = knit::contribution::target(
        &std::env::current_dir().unwrap(),
        &bundle.repos[0],
        forge.as_ref(),
        "main",
        true,
    )
    .unwrap();
    forge
        .merge(
            &target,
            "https://github.com/upstream/widget/pull/7",
            "merge",
            true,
            bundle.repos[0].head_sha.as_deref(),
        )
        .unwrap();
}

#[test]
fn native_merge_cleanup_preserves_upstream_trap_and_absent_source_is_success() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        process::Command,
        sync::{Arc, Mutex},
    };
    let f = Fixture::new();
    f.create();
    let branch = f.bundle()["repos"][0]["featureBranch"]
        .as_str()
        .unwrap()
        .to_owned();
    git(&f.upstream, ["branch", &branch, "main"]);
    let trap = git(&f.upstream, ["rev-parse", &branch]);
    let pr: Value = serde_json::from_slice(&fs::read(f.api.join("pr.json")).unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let fork = f.fork.clone();
    let head = branch.clone();
    let server = std::thread::spawn(move || {
        for _ in 0..7 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let headers = String::from_utf8(request).unwrap();
            let first = headers.lines().next().unwrap().to_string();
            assert!(headers
                .to_ascii_lowercase()
                .contains("authorization: bearer synthetic-native"));
            let length = headers
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|s| s.parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            recorded.lock().unwrap().push(first.clone());
            let (status, body) = if first.starts_with("GET /repos/upstream/widget/pulls/7 ") {
                ("200 OK", pr.to_string())
            } else if first.starts_with("PUT /repos/upstream/widget/pulls/7/merge ") {
                ("200 OK", json!({"merged":true}).to_string())
            } else if first.starts_with("DELETE /repos/contributor/widget/git/refs/heads/") {
                let exists = Command::new("git")
                    .current_dir(&fork)
                    .args(["show-ref", "--verify", &format!("refs/heads/{head}")])
                    .output()
                    .unwrap()
                    .status
                    .success();
                if exists {
                    git(&fork, ["branch", "-D", &head]);
                    ("204 No Content", String::new())
                } else {
                    ("404 Not Found", json!({"message":"Not Found"}).to_string())
                }
            } else {
                panic!("unexpected native request: {first}")
            };
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    for _ in 0..2 {
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "native_merge_provider_child", "--nocapture"])
            .current_dir(&f.workspace)
            .env("CONTRIBUTION_MERGE_ARTIFACT", f.artifact())
            .env("KNIT_HOME", f.root.join("native-home"))
            .env("GH_TOKEN", "synthetic-native")
            .env("KNIT_GITHUB_API_TRANSPORT", "native")
            .env("KNIT_GITHUB_API_BASE", &api)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    // A source-only assignment must be resolved before PUT, even though target
    // reads use the ambient native token. Never inherit target access for DELETE.
    knit(&f.workspace, ["init", "cleanup-auth"]);
    let project_path = f.workspace.join(".knit/projects/cleanup-auth.project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    project["repos"] = json!([{"id":"source", "path":f.checkout, "remote":SOURCE, "baseBranch":"main"}, {"id":"target", "path":f.checkout, "remote":TARGET, "baseBranch":"main"}]);
    fs::write(&project_path, serde_json::to_vec(&project).unwrap()).unwrap();
    let home = f.root.join("native-home");
    fs::create_dir_all(&home).unwrap();
    let key = fs::canonicalize(&project_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fs::write(home.join("forge-auth.json"), serde_json::to_vec(&json!({
        "credentials":{"source":{"provider":"github", "host":"github.com", "tokenEnv":"TEST_SOURCE_UNAVAILABLE"}},
        "scopedCredentials":["source"], "projects":{key.clone():{"source":"source"}}, "ambient":{key:{"target":"github.com/upstream/widget"}}
    })).unwrap()).unwrap();
    let denied = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "native_merge_provider_child", "--nocapture"])
        .current_dir(&f.workspace)
        .env("CONTRIBUTION_MERGE_ARTIFACT", f.artifact())
        .env("KNIT_HOME", home)
        .env_remove("TEST_SOURCE_UNAVAILABLE")
        .env("GH_TOKEN", "synthetic-native")
        .env("KNIT_GITHUB_API_TRANSPORT", "native")
        .env("KNIT_GITHUB_API_BASE", &api)
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("TEST_SOURCE_UNAVAILABLE"),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
    server.join().unwrap();
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("PUT "))
            .count(),
        2
    );
    assert_eq!(git(&f.upstream, ["rev-parse", &branch]), trap);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("DELETE /repos/contributor/"))
            .count(),
        2
    );
}

#[test]
fn ledger_merge_preserves_fallback_source_identity() {
    let f = Fixture::new();
    let mut local: knit::model::ChangeGroup = serde_json::from_value(f.bundle()).unwrap();
    local.repos[0].remote = Some(TARGET.into());
    let mut remote = local.clone();
    remote.repos[0].remote = Some(SOURCE.into());
    remote.repos[0].target_remote = Some(TARGET.into());
    let merged = knit::model::merge_ledgers(&local, &remote, "2026-01-01T00:00:00Z".into());
    assert_eq!(knit::contribution::source(&merged.repos[0]), Some(SOURCE));
    assert_eq!(
        knit::contribution::destination(&merged.repos[0]),
        Some(TARGET)
    );
    assert_eq!(merged.repos[0].remote.as_deref(), Some(TARGET));
}

#[test]
fn fork_force_with_lease_uses_source_tracking_and_preserves_identity() {
    let f = Fixture::new();
    f.create();
    let before = f.bundle();
    let repo: knit::model::RepoEntry = serde_json::from_value(before["repos"][0].clone()).unwrap();
    let reference = knit::contribution::role_ref(&repo, "knit/contribution", true).unwrap();
    let first = git(&f.fork, ["rev-parse", "knit/contribution"]);
    assert_eq!(git(&f.checkout, ["rev-parse", &reference]), first);
    // Fetching a same-named upstream branch must not replace the fork lease.
    git(&f.upstream, ["branch", "knit/contribution", "main"]);
    git(&f.checkout, ["fetch", "origin"]);
    let upstream = git(&f.upstream, ["rev-parse", "knit/contribution"]);
    assert_ne!(upstream, first);
    git(
        &f.checkout,
        ["commit", "--amend", "-m", "Rewritten contribution"],
    );
    let rewritten = git(&f.checkout, ["rev-parse", "HEAD"]);
    let error = f.fail(&["push", "--no-remote"]);
    assert!(error.contains("non-fast-forward"), "{error}");
    assert_eq!(git(&f.checkout, ["rev-parse", &reference]), first);
    f.run(&["push", "--force-with-lease", "--no-remote"]);
    assert_eq!(git(&f.fork, ["rev-parse", "knit/contribution"]), rewritten);
    assert_eq!(git(&f.checkout, ["rev-parse", &reference]), rewritten);
    assert_eq!(
        git(&f.upstream, ["rev-parse", "knit/contribution"]),
        upstream
    );
    assert_eq!(
        f.bundle()["repos"][0]["sourceRemote"],
        before["repos"][0]["sourceRemote"]
    );
    assert_eq!(
        f.bundle()["repos"][0]["targetRemote"],
        before["repos"][0]["targetRemote"]
    );
    assert_eq!(f.bundle()["publications"], before["publications"]);
    assert_eq!(
        f.bundle()["repos"][0]["headSha"],
        git(&f.checkout, ["rev-parse", "HEAD"]).trim()
    );
    // Ordinary pushes also advance the source expectation without -u.
    git(&f.checkout, ["commit", "--allow-empty", "-m", "Follow-up"]);
    f.run(&["push", "--no-remote"]);
    assert_eq!(
        git(&f.checkout, ["rev-parse", &reference]),
        git(&f.checkout, ["rev-parse", "HEAD"])
    );
    git(
        &f.checkout,
        [
            "commit",
            "--amend",
            "--allow-empty",
            "-m",
            "Rewritten follow-up",
        ],
    );
    f.run(&[
        "push",
        "--force-with-lease",
        "--set-upstream",
        "--no-remote",
    ]);
    assert_eq!(
        git(&f.checkout, ["config", "branch.knit/contribution.remote"]).trim(),
        SOURCE
    );
    assert_eq!(
        f.bundle()["repos"][0]["sourceRemote"],
        before["repos"][0]["sourceRemote"]
    );
    assert_eq!(
        f.bundle()["repos"][0]["targetRemote"],
        before["repos"][0]["targetRemote"]
    );
    assert_eq!(f.bundle()["publications"], before["publications"]);
    assert_eq!(
        f.bundle()["repos"][0]["headSha"],
        git(&f.checkout, ["rev-parse", "HEAD"]).trim()
    );
}

#[test]
fn fork_force_with_lease_rejects_unknown_source_even_when_origin_matches() {
    let f = Fixture::new();
    f.run(&["push", "--no-remote"]);
    let before = f.bundle();
    let repo: knit::model::RepoEntry = serde_json::from_value(before["repos"][0].clone()).unwrap();
    let reference = knit::contribution::role_ref(&repo, "knit/contribution", true).unwrap();
    let observed = git(&f.checkout, ["rev-parse", &reference]);
    // Another writer changes the fork, then an upstream fetch matches that tip.
    let moved = f.foreign_fork_commit();
    git(
        &f.fork,
        ["update-ref", "refs/heads/knit/contribution", moved.trim()],
    );
    git(
        &f.upstream,
        [
            "fetch",
            f.fork.to_str().unwrap(),
            "refs/heads/knit/contribution:refs/heads/knit/contribution",
        ],
    );
    git(&f.checkout, ["fetch", "origin"]);
    assert_eq!(
        git(
            &f.checkout,
            ["rev-parse", "refs/remotes/origin/knit/contribution"]
        ),
        moved
    );
    git(
        &f.checkout,
        ["commit", "--amend", "-m", "Rewritten contribution"],
    );
    let error = f.fail(&["push", "--force-with-lease", "--no-remote"]);
    assert!(error.contains("this bundle never recorded"), "{error}");
    assert!(error.contains(moved.trim()), "{error}");
    assert!(
        error.contains(&format!("git fetch {SOURCE} knit/contribution")),
        "{error}"
    );
    assert_eq!(git(&f.fork, ["rev-parse", "knit/contribution"]), moved);
    assert_eq!(git(&f.checkout, ["rev-parse", &reference]), observed);
    assert_eq!(f.bundle(), before);
}

#[test]
fn fork_force_with_lease_without_receipt_creates_missing_branch_and_accepts_recorded_tip() {
    let f = Fixture::new();
    f.run(&["push", "--force-with-lease", "--no-remote"]);
    let repo: knit::model::RepoEntry =
        serde_json::from_value(f.bundle()["repos"][0].clone()).unwrap();
    let reference = knit::contribution::role_ref(&repo, "knit/contribution", true).unwrap();
    let first = git(&f.fork, ["rev-parse", "knit/contribution"]);
    f.clear_push_receipts();
    git(
        &f.checkout,
        ["commit", "--amend", "-m", "Rewritten contribution"],
    );
    assert_eq!(f.bundle()["repos"][0]["headSha"], first.trim());
    let output = f.run(&["push", "--force-with-lease", "--no-remote"]);
    assert!(output.contains("no Knit push receipt"), "{output}");
    assert!(
        output.contains("leasing against the remote tip"),
        "{output}"
    );
    assert!(output.contains(&first.trim()[..7]), "{output}");
    let rewritten = git(&f.checkout, ["rev-parse", "HEAD"]);
    assert_ne!(rewritten, first);
    assert_eq!(git(&f.fork, ["rev-parse", "knit/contribution"]), rewritten);
    assert_eq!(git(&f.checkout, ["rev-parse", &reference]), rewritten);
    assert_eq!(f.bundle()["repos"][0]["headSha"], rewritten.trim());
}

#[test]
fn fork_plain_force_is_still_refused() {
    let f = Fixture::new();
    f.run(&["push", "--no-remote"]);
    let first = git(&f.fork, ["rev-parse", "knit/contribution"]);
    git(
        &f.checkout,
        ["commit", "--amend", "-m", "Rewritten contribution"],
    );
    let error = f.fail(&["push", "--force", "--no-remote"]);
    assert!(
        error.contains("force push is not supported for cross-repository contributions"),
        "{error}"
    );
    assert_eq!(git(&f.fork, ["rev-parse", "knit/contribution"]), first);
}

#[test]
fn fork_force_with_lease_uses_dedicated_fork_remote_tracking() {
    let f = Fixture::new();
    f.run(&["push", "--no-remote"]);
    git(
        &f.checkout,
        ["remote", "set-url", "--push", "origin", TARGET],
    );
    git(&f.checkout, ["remote", "add", "fork", SOURCE]);
    // A Knit source fetch must refresh both the role ref and the dedicated
    // fork tracking ref, even when the latter is initially absent.
    git(
        &f.checkout,
        ["commit", "--allow-empty", "-m", "Fork advances"],
    );
    git(&f.checkout, ["push", "fork", "knit/contribution"]);
    git(
        &f.checkout,
        ["update-ref", "-d", "refs/remotes/fork/knit/contribution"],
    );
    let repo: knit::model::RepoEntry =
        serde_json::from_value(f.bundle()["repos"][0].clone()).unwrap();
    knit::contribution::fetch_ref(&f.checkout, &repo, "knit/contribution", true).unwrap();
    assert!(git_success(
        &f.checkout,
        [
            "show-ref",
            "--verify",
            "refs/remotes/fork/knit/contribution"
        ]
    ));
    git(
        &f.checkout,
        [
            "commit",
            "--amend",
            "--allow-empty",
            "-m",
            "Rewritten contribution",
        ],
    );
    f.run(&["push", "--force-with-lease", "--no-remote"]);
    let observed = git(&f.fork, ["rev-parse", "knit/contribution"]);
    assert_eq!(
        git(
            &f.checkout,
            ["rev-parse", "refs/remotes/fork/knit/contribution"]
        ),
        observed
    );
    let moved = f.foreign_fork_commit();
    git(
        &f.fork,
        ["update-ref", "refs/heads/knit/contribution", moved.trim()],
    );
    git(
        &f.checkout,
        [
            "commit",
            "--amend",
            "--allow-empty",
            "-m",
            "Another rewrite",
        ],
    );
    let error = f.fail(&["push", "--force-with-lease", "--no-remote"]);
    assert!(error.contains("this bundle never recorded"), "{error}");
    assert!(error.contains(moved.trim()), "{error}");
    assert!(
        error.contains(&format!("git fetch {SOURCE} knit/contribution")),
        "{error}"
    );
    assert_eq!(git(&f.fork, ["rev-parse", "knit/contribution"]), moved);
}

#[test]
fn fork_force_with_lease_shares_observation_across_transport_spellings() {
    check_fork_lease_transport_spelling(false);
}

#[test]
fn fork_force_with_lease_migrates_recorded_source_legacy_observation() {
    check_fork_lease_transport_spelling(true);
}

fn check_fork_lease_transport_spelling(legacy: bool) {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    f.run(&["push", "--no-remote"]);
    let before = f.bundle();
    let repo: knit::model::RepoEntry = serde_json::from_value(before["repos"][0].clone()).unwrap();
    let reference = knit::contribution::role_ref(&repo, "knit/contribution", true).unwrap();
    let observed = git(&f.fork, ["rev-parse", "knit/contribution"]);
    if legacy {
        f.clear_push_receipts();
        let old_ref = format!(
            "refs/knit/contributions/{:x}",
            Sha256::digest(format!("{SOURCE}\0knit/contribution").as_bytes())
        );
        git(&f.checkout, ["update-ref", &old_ref, observed.trim()]);
    } else {
        f.run(&["pull", "--feature", "--no-remote"]);
    }
    let ssh_source = "git@github.com:contributor/widget.git";
    git(
        &f.checkout,
        [
            "config",
            "--add",
            &format!("url.{}.insteadOf", f.fork.display()),
            ssh_source,
        ],
    );
    git(
        &f.checkout,
        ["remote", "set-url", "--push", "origin", ssh_source],
    );
    let mut alias = repo.clone();
    alias.source_remote = Some(ssh_source.into());
    assert_eq!(
        knit::contribution::role_ref(&alias, "knit/contribution", true).unwrap(),
        reference
    );
    git(
        &f.checkout,
        [
            "commit",
            "--amend",
            "-m",
            "Rewritten transport contribution",
        ],
    );
    let output = f.run(&["push", "--force-with-lease", "--no-remote"]);
    if legacy {
        assert!(output.contains("no Knit push receipt"), "{output}");
    }
    assert_eq!(
        git(&f.fork, ["rev-parse", "knit/contribution"]),
        git(&f.checkout, ["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(&f.checkout, ["rev-parse", &reference]),
        git(&f.checkout, ["rev-parse", "HEAD"])
    );
    assert_eq!(
        f.bundle()["repos"][0]["sourceRemote"],
        before["repos"][0]["sourceRemote"]
    );
    assert_eq!(
        f.bundle()["repos"][0]["targetRemote"],
        before["repos"][0]["targetRemote"]
    );
    // Transport aliases and migrated receipts must still reject an unknown tip.
    let before_rejection = f.bundle();
    let receipt = git(&f.checkout, ["rev-parse", &reference]);
    let moved = f.foreign_fork_commit();
    git(
        &f.fork,
        ["update-ref", "refs/heads/knit/contribution", moved.trim()],
    );
    let error = f.fail(&["push", "--force-with-lease", "--no-remote"]);
    assert!(error.contains("this bundle never recorded"), "{error}");
    assert!(error.contains(moved.trim()), "{error}");
    assert!(
        error.contains(&format!("git fetch {ssh_source} knit/contribution")),
        "{error}"
    );
    assert_eq!(git(&f.fork, ["rev-parse", "knit/contribution"]), moved);
    assert_eq!(git(&f.checkout, ["rev-parse", &reference]), receipt);
    assert_eq!(f.bundle(), before_rejection);
}

#[test]
fn fork_force_with_lease_observes_native_then_knit_fetch_after_canonical_receipt() {
    let f = Fixture::new();
    f.run(&["push", "--no-remote"]);
    git(
        &f.checkout,
        ["remote", "set-url", "--push", "origin", TARGET],
    );
    git(&f.checkout, ["remote", "add", "fork", SOURCE]);
    let repo: knit::model::RepoEntry =
        serde_json::from_value(f.bundle()["repos"][0].clone()).unwrap();
    let reference = knit::contribution::role_ref(&repo, "knit/contribution", true).unwrap();
    let writer = f.root.join("fork-writer");
    git(
        &f.root,
        ["clone", f.fork.to_str().unwrap(), writer.to_str().unwrap()],
    );
    configure_git_user(&writer);
    git(&writer, ["checkout", "knit/contribution"]);
    for native in [true, false] {
        git(&writer, ["fetch", "origin"]);
        git(&writer, ["reset", "--hard", "origin/knit/contribution"]);
        git(
            &writer,
            ["commit", "--allow-empty", "-m", "Fork writer advances"],
        );
        git(&writer, ["push", "origin", "knit/contribution"]);
        let latest = git(&f.fork, ["rev-parse", "knit/contribution"]);
        assert_ne!(git(&f.checkout, ["rev-parse", &reference]), latest);
        if native {
            git(&f.checkout, ["fetch", "fork"]);
            // Native fetching leaves the canonical receipt unchanged.
            assert_ne!(git(&f.checkout, ["rev-parse", &reference]), latest);
        } else {
            // The next Knit observation must refresh the older native receipt.
            assert_ne!(
                git(
                    &f.checkout,
                    ["rev-parse", "refs/remotes/fork/knit/contribution"]
                ),
                latest
            );
            f.run(&["pull", "--feature", "--no-remote"]);
        }
        assert_eq!(
            git(
                &f.checkout,
                ["rev-parse", "refs/remotes/fork/knit/contribution"]
            ),
            latest
        );
        // Recording the fetched tip in the feature reflog makes it safe to
        // lease even when the canonical receipt still names an older tip.
        git(&f.checkout, ["reset", "--hard", latest.trim()]);
        git(
            &f.checkout,
            [
                "commit",
                "--amend",
                "--allow-empty",
                "-m",
                "Rewritten observed contribution",
            ],
        );
        f.run(&["push", "--force-with-lease", "--no-remote"]);
        assert_eq!(
            git(&f.fork, ["rev-parse", "knit/contribution"]),
            git(&f.checkout, ["rev-parse", "HEAD"])
        );
    }
}

#[test]
fn fork_source_fetch_does_not_rewrite_custom_tracking_destinations() {
    let f = Fixture::new();
    f.run(&["push", "--no-remote"]);
    git(&f.checkout, ["remote", "add", "fork", SOURCE]);
    git(
        &f.checkout,
        [
            "config",
            "remote.fork.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    git(&f.upstream, ["branch", "knit/contribution", "main"]);
    git(&f.checkout, ["fetch", "origin"]);
    let trap = git(
        &f.checkout,
        ["rev-parse", "refs/remotes/origin/knit/contribution"],
    );
    let repo: knit::model::RepoEntry =
        serde_json::from_value(f.bundle()["repos"][0].clone()).unwrap();
    knit::contribution::fetch_ref(&f.checkout, &repo, "knit/contribution", true).unwrap();
    assert_eq!(
        git(
            &f.checkout,
            ["rev-parse", "refs/remotes/origin/knit/contribution"]
        ),
        trap
    );
    assert!(!git_success(
        &f.checkout,
        [
            "show-ref",
            "--verify",
            "refs/remotes/fork/knit/contribution"
        ]
    ));
}
