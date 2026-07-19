mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;

#[test]
fn doctor_ignores_missing_recorded_worktree_only_for_archived_bundles() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["bundle", "archived checkout"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    knit(
        &workspace,
        ["bundle", "archive", "archived-checkout", "--keep-worktrees"],
    );

    let bundle_path = workspace.join(".knit/bundles/archived-checkout.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    let missing_worktree = ".knit/worktrees/archived-checkout/missing";
    bundle["repos"][0]["worktreePath"] = json!(missing_worktree);
    fs::write(
        &bundle_path,
        format!("{}\n", serde_json::to_string_pretty(&bundle).unwrap()),
    )
    .unwrap();

    assert!(knit(&workspace, ["doctor"]).contains("Knit doctor: ok"));

    bundle["state"] = json!("open");
    fs::write(
        &bundle_path,
        format!("{}\n", serde_json::to_string_pretty(&bundle).unwrap()),
    )
    .unwrap();
    let open_doctor = knit_fails(&workspace, ["doctor"]);
    assert!(open_doctor.contains("worktree missing"), "{open_doctor}");
    assert!(open_doctor.contains(missing_worktree), "{open_doctor}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_start_and_add_support_ad_hoc_work() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "ad hoc"]);
    assert!(knit(&workspace, ["bundle"]).contains("Bundle: ad-hoc"));
    let empty_bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/ad-hoc.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(empty_bundle["repos"].as_array().unwrap().len(), 0);

    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    assert!(workspace.join(".knit/worktrees/ad-hoc/backend").exists());
    let bundle_agents =
        fs::read_to_string(workspace.join(".knit/worktrees/ad-hoc/AGENTS.md")).unwrap();
    assert!(bundle_agents.contains("bundle `ad-hoc`"));
    assert!(!bundle_agents.contains("knit --bundle"));
    assert!(!workspace
        .join(".knit/worktrees/ad-hoc/backend/AGENTS.md")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_does_not_duplicate_ledger_commits_when_head_projection_is_stale() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    fs::write(
        workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "feature\n",
    )
    .unwrap();
    knit(
        &workspace,
        ["commit", "--all", "-m", "Add venue capacity integration"],
    );

    let bundle_path = workspace.join(".knit/bundles/venue-capacity.bundle.json");
    let mut bundle = read_bundle(&workspace);
    let base_sha = bundle["repos"][0]["baseSha"].as_str().unwrap().to_string();
    let group_sha = bundle["commitGroups"][0]["commits"][0]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        bundle["repos"][0]["headSha"].as_str(),
        Some(group_sha.as_str())
    );
    assert_eq!(
        bundle["commitGroups"][0]["author"]["email"].as_str(),
        Some("knit@example.test"),
        "commit group should record the git author email"
    );
    assert_eq!(
        bundle["commitGroups"][0]["author"]["name"].as_str(),
        Some("Knit Smoke")
    );

    bundle["repos"][0]["headSha"] = json!(base_sha);
    fs::write(
        &bundle_path,
        format!("{}\n", serde_json::to_string_pretty(&bundle).unwrap()),
    )
    .unwrap();

    let status = knit(&workspace, ["status"]);
    assert!(!status.contains("unrecorded commits"));

    let sync = knit(&workspace, ["sync"]);
    assert!(sync.contains("No unrecorded git commits found."));
    let log = knit(&workspace, ["log"]);
    assert!(log.contains("Add venue capacity integration"));
    assert!(!log.contains("observed git changes"));

    let doctor = knit_fails(&workspace, ["doctor"]);
    assert!(doctor.contains("headSha projection differs from ledger"));
    let migrate_check = knit_fails(&workspace, ["migrate", "--check"]);
    assert!(migrate_check.contains("need migration"));
    knit(&workspace, ["migrate"]);
    let repaired = read_bundle(&workspace);
    assert_eq!(
        repaired["repos"][0]["headSha"].as_str(),
        Some(group_sha.as_str())
    );
    assert!(knit(&workspace, ["doctor"]).contains("Knit doctor: ok"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_context_supports_parallel_worktrees_and_workspace_switches() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    let subdir = workspace.join("subdir");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&subdir).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "fix a"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    knit(&workspace, ["bundle", "fix b"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    let fix_a = workspace.join(".knit/worktrees/fix-a/backend");
    let fix_b = workspace.join(".knit/worktrees/fix-b/backend");
    assert!(fix_a.exists());
    assert!(fix_b.exists());
    assert_eq!(
        git(&fix_a, ["branch", "--show-current"]).trim(),
        "knit/fix-a"
    );
    assert_eq!(
        git(&fix_b, ["branch", "--show-current"]).trim(),
        "knit/fix-b"
    );

    let fix_a_status = knit(&fix_a, ["status"]);
    assert!(fix_a_status.contains("Bundle: fix-a (cwd)"));
    let fix_b_status = knit(&fix_b, ["status"]);
    assert!(fix_b_status.contains("Bundle: fix-b (cwd)"));

    assert!(knit(&workspace, ["--bundle", "fix-b", "status"]).contains("Bundle: fix-b (explicit)"));
    assert!(
        knit_with_env(&workspace, ["status"], &[("KNIT_BUNDLE", "fix-b")])
            .contains("Bundle: fix-b (env)")
    );

    assert!(knit_fails(&workspace, ["switch", "fix-a"]).contains("without --workspace"));
    assert!(knit_fails(&subdir, ["switch", "fix-a"]).contains("without --workspace"));
    knit(&workspace, ["switch", "fix-a", "--workspace"]);
    assert!(knit_fails(&workspace, ["status"]).contains("multiple open bundles"));
    assert!(knit(&workspace, ["--bundle", "fix-a", "status"]).contains("Bundle: fix-a (explicit)"));

    // Generated worktrees still resolve their owning bundle from the path,
    // independent of the shared workspace fallback.
    assert!(knit(&fix_b, ["status"]).contains("Bundle: fix-b (cwd)"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn commit_from_worktree_uses_worktree_bundle_not_workspace_fallback() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "test"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    knit(&workspace, ["bundle", "test2"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    assert!(knit_fails(&workspace, ["status"]).contains("multiple open bundles"));

    let test_checkout = workspace.join(".knit/worktrees/test/backend");
    fs::write(test_checkout.join("test.md"), "test\n").unwrap();
    assert!(
        knit_fails(&workspace, ["commit", "--all", "-m", "Add test"])
            .contains("multiple open bundles")
    );

    let commit = knit(&test_checkout, ["commit", "--all", "-m", "Add test"]);
    assert!(commit.contains("Recorded commit group"));

    let test_bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/test.bundle.json")).unwrap(),
    )
    .unwrap();
    let test2_bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/test2.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(test_bundle["commitGroups"].as_array().unwrap().len(), 1);
    assert_eq!(test2_bundle["commitGroups"].as_array().unwrap().len(), 0);
    assert!(test_checkout.join("test.md").exists());
    assert!(!workspace
        .join(".knit/worktrees/test2/backend/test.md")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stealth_config_suppresses_knit_trailers_in_git_commits() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "quiet work"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let checkout = workspace.join(".knit/worktrees/quiet-work/backend");

    append_line(&checkout.join("app.txt"), "default commit");
    knit(&workspace, ["commit", "--all", "-m", "Default commit"]);
    let default_message = git(&checkout, ["log", "-1", "--format=%B"]);
    assert!(default_message.contains("Default commit"));
    assert!(default_message.contains("Knit-Group:"), "{default_message}");
    assert!(
        default_message.contains("Knit-Bundle:"),
        "{default_message}"
    );

    let set = knit(&workspace, ["config", "set", "stealth", "true"]);
    assert!(set.contains("stealth=true"), "{set}");

    append_line(&checkout.join("app.txt"), "stealth commit");
    knit(&workspace, ["commit", "--all", "-m", "Stealth commit"]);
    let stealth_message = git(&checkout, ["log", "-1", "--format=%B"]);
    assert_eq!(stealth_message.trim(), "Stealth commit");

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/quiet-work.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        bundle["commitGroups"][1]["message"].as_str(),
        Some("Stealth commit")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn run_executes_named_project_commands_in_bundle_worktrees() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        [
            "project",
            "command",
            "set",
            "show-root",
            "--repo",
            "backend",
            "--",
            "git",
            "rev-parse",
            "--show-toplevel",
        ],
    );
    assert!(knit(&workspace, ["project", "command", "list"]).contains("show-root"));

    knit(&workspace, ["bundle", "run feature"]);
    let worktree = workspace.join(".knit/worktrees/run-feature/backend");
    let named = knit(&workspace, ["run", "show-root"]);
    // git prints forward-slash paths on every platform; normalize both sides
    // (and case, for Windows) before comparing.
    let named_normalized = named.replace('\\', "/").to_lowercase();
    let worktree_normalized = worktree.to_string_lossy().replace('\\', "/").to_lowercase();
    assert!(named_normalized.contains(&worktree_normalized), "{named}");

    let raw = knit(
        &workspace,
        [
            "run",
            "--repo",
            "backend",
            "--",
            "git",
            "branch",
            "--show-current",
        ],
    );
    assert_eq!(raw.trim(), "knit/run-feature");
    assert!(knit(&workspace, ["run", "--list"]).contains("show-root"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_creation_refuses_slug_taken_on_sync_remote() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    let base_url = spawn_fake_knithub_export("payment-flow", "open");
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let refused = knit_fails_with_env(&workspace, ["bundle", "payment flow"], &env);
    assert!(refused.contains("already exists on the sync remote"));
    assert!(!workspace
        .join(".knit/bundles/payment-flow.bundle.json")
        .exists());

    // A different title passes the check, and --force overrides it.
    let created = knit_with_env(&workspace, ["bundle", "other feature"], &env);
    assert!(created.contains("Active bundle:"));
    let forced = knit_with_env(&workspace, ["bundle", "payment flow", "--force"], &env);
    assert!(forced.contains("Active bundle:"));
    assert!(workspace
        .join(".knit/bundles/payment-flow.bundle.json")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stale_bundle_lock_from_dead_process_is_reclaimed() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    // A crashed knit process left its lock behind: record a pid that is
    // guaranteed dead by the time the next command runs.
    let dead_pid = exited_process_pid();
    let lock_dir = workspace.join(".knit/locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let lock_path = lock_dir.join("venue-capacity.lock");
    fs::write(&lock_path, dead_pid.to_string()).unwrap();

    // The stale lock is reclaimed instead of demanding manual cleanup.
    let sync = knit(&workspace, ["sync"]);
    assert!(sync.contains("No unrecorded git commits"));

    // A lock held by a live process still blocks, and names the holder.
    fs::write(&lock_path, std::process::id().to_string()).unwrap();
    let blocked = knit_fails(&workspace, ["sync"]);
    assert!(blocked.contains("Another Knit process"));
    assert!(blocked.contains(&format!("(pid {})", std::process::id())));
    fs::remove_file(&lock_path).unwrap();

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ledger_nodes_record_ambient_session_identity() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    // An agent harness exports KNIT_SESSION per conversation; every ledger
    // node written by commands run in that session carries it.
    let env = [("KNIT_SESSION", "k3-thread-123")];
    knit_with_env(&workspace, ["bundle", "venue capacity"], &env);
    knit_with_env(
        &workspace,
        ["bundle", "add", backend.to_str().unwrap()],
        &env,
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "session work",
    );
    knit_with_env(
        &workspace,
        ["commit", "--all", "-m", "Session-attributed work"],
        &env,
    );

    let bundle = read_bundle(&workspace);
    let nodes = bundle["nodes"].as_array().unwrap();
    assert!(nodes.len() >= 3);
    for node in nodes {
        assert_eq!(
            node["sessionId"].as_str(),
            Some("k3-thread-123"),
            "node {} missing sessionId",
            node["id"]
        );
    }

    // Plain CLI use (no KNIT_SESSION) stays unattributed.
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "human work",
    );
    knit(&workspace, ["commit", "--all", "-m", "Unattributed work"]);
    let bundle = read_bundle(&workspace);
    let last = bundle["nodes"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["type"].as_str(), Some("commit.group"));
    assert!(last.get("sessionId").is_none());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ledger_nodes_record_ambient_actor_identity() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    // On a shared environment the server exports the acting human per
    // provider session alongside KNIT_SESSION; ledger nodes record both.
    let env = [
        ("KNIT_SESSION", "k3-thread-456"),
        ("T3_ACTOR_SESSION", "session-abc"),
        ("T3_ACTOR_LABEL", "alice"),
        ("T3_ACTOR_EMAIL", "alice@example.com"),
    ];
    knit_with_env(&workspace, ["bundle", "venue capacity"], &env);
    knit_with_env(
        &workspace,
        ["bundle", "add", backend.to_str().unwrap()],
        &env,
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "actor work",
    );
    knit_with_env(
        &workspace,
        ["commit", "--all", "-m", "Actor-attributed work"],
        &env,
    );

    let bundle = read_bundle(&workspace);
    let nodes = bundle["nodes"].as_array().unwrap();
    assert!(nodes.len() >= 3);
    for node in nodes {
        assert_eq!(
            node["actor"]["session"].as_str(),
            Some("session-abc"),
            "node {} missing actor session",
            node["id"]
        );
        assert_eq!(node["actor"]["label"].as_str(), Some("alice"));
        assert_eq!(node["actor"]["email"].as_str(), Some("alice@example.com"));
    }

    // Label and email are optional; a bare actor session still records.
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "bare actor work",
    );
    knit_with_env(
        &workspace,
        ["commit", "--all", "-m", "Bare actor work"],
        &[("T3_ACTOR_SESSION", "session-bare")],
    );
    let bundle = read_bundle(&workspace);
    let last = bundle["nodes"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["actor"]["session"].as_str(), Some("session-bare"));
    assert!(last["actor"].get("label").is_none());
    assert!(last["actor"].get("email").is_none());

    // Plain CLI use stays unattributed.
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "human work",
    );
    knit(&workspace, ["commit", "--all", "-m", "Unattributed work"]);
    let bundle = read_bundle(&workspace);
    let last = bundle["nodes"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["type"].as_str(), Some("commit.group"));
    assert!(last.get("actor").is_none());

    fs::remove_dir_all(root).unwrap();
}

// Regression: `knit bundle add` must record the repo in the artifact before it
// creates any git side effects, so an interruption during materialization (a
// SIGPIPE from `| head`, a Ctrl-C, or a crash) can never leave a branch and
// worktree the bundle artifact never recorded. We force materialization to fail
// deterministically by pre-planting a non-git directory where the worktree would
// go, then assert the repo is already in the artifact even though the command
// failed — and that `knit bundle worktree` recovers the missing checkout.
#[test]
fn bundle_add_records_repo_before_materializing_worktree() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["bundle", "crash consistency"]);

    // A plain (non-git) directory at the worktree target makes materialization
    // bail with "exists but is not a git worktree" — a clean stand-in for any
    // interruption after the artifact-first save but before materialization
    // finishes.
    let blocker = workspace.join(".knit/worktrees/crash-consistency/backend");
    fs::create_dir_all(&blocker).unwrap();
    fs::write(blocker.join("stray.txt"), "not a worktree\n").unwrap();

    let failure = knit_fails(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    assert!(
        failure.contains("worktree"),
        "expected materialization to fail, got: {failure}"
    );

    // Crash-consistency invariant: the artifact already lists the repo even
    // though the command died during materialization.
    assert!(
        bundle_repo_ids(&workspace, "crash-consistency").contains(&"backend".to_string()),
        "bundle add must persist the repo entry before materializing worktrees"
    );

    // The entry is recorded but not yet materialized (recoverable state).
    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/crash-consistency.bundle.json")).unwrap(),
    )
    .unwrap();
    assert!(
        bundle["repos"][0]["worktreePath"].is_null(),
        "unmaterialized repo should have no recorded worktree path"
    );

    // Recovery: clear the blocker and rematerialize the missing checkout.
    fs::remove_dir_all(&blocker).unwrap();
    knit(&workspace, ["bundle", "worktree"]);
    assert!(blocker.join("app.txt").exists());
    let recovered: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/crash-consistency.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        recovered["repos"][0]["worktreePath"].as_str(),
        Some(".knit/worktrees/crash-consistency/backend")
    );

    fs::remove_dir_all(root).unwrap();
}
