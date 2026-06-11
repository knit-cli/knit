mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;

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
    let worktree_agents =
        fs::read_to_string(workspace.join(".knit/worktrees/ad-hoc/backend/AGENTS.md")).unwrap();
    assert!(worktree_agents.contains("bundle `ad-hoc`"));
    assert!(!worktree_agents.contains("knit --bundle"));

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
fn run_executes_named_project_commands_in_bundle_worktrees() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["init", "arbient"]);
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
    let worktree_normalized = worktree
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
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
    knit(&workspace, ["remote", "add", "knithub", &base_url]);
    let env = [("KNITHUB_TOKEN", "test-token")];

    let refused = knit_fails_with_env(&workspace, ["bundle", "payment flow"], &env);
    assert!(refused.contains("already exists on the KnitHub sync remote"));
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
