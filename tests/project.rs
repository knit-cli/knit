mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;

#[test]
fn init_can_generate_agents_tutorial() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let output = knit(&workspace, ["bundle", "venue capacity", "--agents"]);
    assert!(output.contains("AGENTS.md"));

    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("This is a Knit workspace"));
    assert!(agents.contains("knit status"));
    assert!(agents.contains("knit init"));
    assert!(agents.contains("knit bundle \""));
    assert!(agents.contains("fetches each selected repo's configured `origin/<baseBranch>`"));
    assert!(agents.contains("knit workspace status"));
    assert!(agents.contains("knit pull --base"));
    assert!(agents.contains("knit pull --current"));
    assert!(agents.contains("knit project set-base"));
    assert!(agents.contains("knit bundle add"));
    assert!(agents.contains("knit bundle remove <repo>"));
    assert!(agents.contains("knit bundle prune"));
    assert!(agents.contains("knit bundle prune --apply --worktrees --branches"));
    assert!(agents.contains("knit bundle prune --apply --all"));
    assert!(agents.contains("--remote-branches"));
    assert!(agents.contains("matching remote bundle records"));
    assert!(agents.contains("archived (never deleted) with the everyday `bundle:push` scope"));
    assert!(agents.contains("knit project remove <project> --force"));
    assert!(agents.contains("knit --bundle feature-a commit"));
    assert!(agents.contains("knit --bundle feature-a commit --all"));
    assert!(agents.contains("knit --bundle feature-a push --set-upstream"));
    assert!(agents.contains("If the harness provides subagents or agent teams"));
    assert!(agents.contains("minimum capable subagent/model"));
    assert!(agents.contains("Project JSON can define a default `landing` template"));
    assert!(agents.contains(".knit/land-plans/<bundle>.land.json"));
    assert!(agents.contains("urdir review --bundle"));
    assert!(agents.contains("gloss view --review"));

    fs::remove_file(workspace.join("AGENTS.md")).unwrap();
    let existing_workspace_output = knit(&workspace, ["bundle", "venue capacity", "--agents"]);
    assert!(existing_workspace_output.contains("AGENTS.md"));
    let existing_workspace_agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(existing_workspace_agents.contains("This is a Knit workspace"));

    fs::write(workspace.join("AGENTS.md"), "custom guidance\n").unwrap();
    knit(
        &workspace,
        ["bundle", "venue capacity", "--force", "--agents"],
    );
    let updated = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(updated.contains("custom guidance"));
    assert!(updated.contains("This is a Knit workspace"));
    assert_eq!(updated.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);

    knit(
        &workspace,
        ["bundle", "venue capacity", "--force", "--agents"],
    );
    let rerun = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert_eq!(rerun.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_default_repos_start_bundle_without_track() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let scraper = root.join("scraper");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    init_repo(&scraper, "scraper");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );
    knit(
        &workspace,
        [
            "project",
            "add",
            "scraper",
            scraper.to_str().unwrap(),
            "--observe",
        ],
    );
    assert!(knit(&workspace, ["project", "list"]).contains("demo"));
    assert!(knit(&workspace, ["project", "show"]).contains("\"id\": \"demo\""));

    knit(&workspace, ["bundle", "project feature"]);

    assert!(workspace
        .join(".knit/worktrees/project-feature/backend")
        .exists());
    assert!(workspace
        .join(".knit/worktrees/project-feature/frontend")
        .exists());
    assert!(!workspace
        .join(".knit/worktrees/project-feature/scraper")
        .exists());

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/project-feature.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bundle["projectId"].as_str(), Some("demo"));
    assert_eq!(bundle["repos"].as_array().unwrap().len(), 2);
    assert_eq!(bundle["repos"][0]["id"].as_str(), Some("backend"));
    assert_eq!(bundle["repos"][1]["id"].as_str(), Some("frontend"));

    let list = knit(&workspace, ["bundle", "list"]);
    assert!(list.contains("project-feature"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_add_infers_nonstandard_cached_remote_default() {
    let root = unique_temp_dir();
    let (remote, movida, _collaborator) = init_remote_repo(&root, "movida");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    git(&movida, ["branch", "master"]);
    git(&movida, ["checkout", "-b", "stable"]);
    git(&movida, ["push", "--set-upstream", "origin", "stable"]);
    git(&remote, ["symbolic-ref", "HEAD", "refs/heads/stable"]);
    git(&movida, ["remote", "set-head", "origin", "-a"]);

    knit(&workspace, ["init", "demo"]);
    let added = knit(
        &workspace,
        ["project", "add", "movida", movida.to_str().unwrap()],
    );
    assert!(added.contains("stable"), "{added}");
    assert!(added.contains("origin/HEAD"), "{added}");

    let project: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/projects/demo.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(project["repos"][0]["baseBranch"].as_str(), Some("stable"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_add_queries_live_remote_default_when_origin_head_is_not_cached() {
    let root = unique_temp_dir();
    let (remote, movida, _collaborator) = init_remote_repo(&root, "movida");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    git(&movida, ["checkout", "-b", "stable"]);
    git(&movida, ["push", "--set-upstream", "origin", "stable"]);
    git(&remote, ["symbolic-ref", "HEAD", "refs/heads/stable"]);
    git(
        &movida,
        ["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
    );

    knit(&workspace, ["init", "demo"]);
    let added = knit(
        &workspace,
        ["project", "add", "movida", movida.to_str().unwrap()],
    );
    assert!(added.contains("stable"), "{added}");
    assert!(added.contains("origin's default branch"), "{added}");

    let project: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/projects/demo.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(project["repos"][0]["baseBranch"].as_str(), Some("stable"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_set_base_preserves_repo_settings_and_keeps_open_bundles_pinned() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    git(&backend, ["checkout", "-b", "stable"]);
    git(&backend, ["push", "--set-upstream", "origin", "stable"]);
    git(&backend, ["checkout", "main"]);

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        [
            "project",
            "add",
            "backend",
            backend.to_str().unwrap(),
            "--observe",
        ],
    );
    knit(
        &workspace,
        ["bundle", "existing feature", "--repo", "backend"],
    );

    let changed = knit(&workspace, ["project", "set-base", "backend", "stable"]);
    assert!(changed.contains("main"), "{changed}");
    assert!(changed.contains("stable"), "{changed}");
    assert!(
        changed.contains("existing bundles remain pinned"),
        "{changed}"
    );
    assert!(changed.contains("existing-feature"), "{changed}");

    let project: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/projects/demo.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(project["repos"][0]["baseBranch"].as_str(), Some("stable"));
    assert_eq!(
        project["repos"][0]["includeByDefault"].as_bool(),
        Some(false)
    );

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/existing-feature.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bundle["repos"][0]["baseBranch"].as_str(), Some("main"));

    let invalid = knit_fails(
        &workspace,
        ["project", "set-base", "backend", "does-not-exist"],
    );
    assert!(invalid.contains("was not found"), "{invalid}");
    let unchanged: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/projects/demo.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(unchanged["repos"][0]["baseBranch"].as_str(), Some("stable"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn untouched_bundle_repo_can_be_recreated_on_a_corrected_unrelated_base() {
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
    knit(&workspace, ["bundle", "corrected base"]);
    let old_base = git(&backend, ["rev-parse", "main"]);

    git(&backend, ["checkout", "--orphan", "stable"]);
    fs::write(backend.join("app.txt"), "unrelated stable root\n").unwrap();
    git(&backend, ["add", "--all"]);
    git(&backend, ["commit", "-m", "Stable root"]);
    let stable = git(&backend, ["rev-parse", "stable"]);

    knit(&workspace, ["project", "set-base", "backend", "stable"]);
    knit(
        &workspace,
        ["bundle", "remove", "backend", "--delete-branch"],
    );
    assert!(git(&backend, ["branch", "--list", "knit/corrected-base"])
        .trim()
        .is_empty());

    knit(&workspace, ["bundle", "add", "backend"]);
    let checkout = workspace.join(".knit/worktrees/corrected-base/backend");
    assert_eq!(git(&checkout, ["rev-parse", "HEAD"]), stable);
    assert_ne!(git(&checkout, ["rev-parse", "HEAD"]), old_base);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_bundle_starts_from_fresh_remote_base_without_moving_dirty_source_checkout() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let stale_local = git(&backend, ["rev-parse", "main"]);

    append_line(&collaborator.join("app.txt"), "fresh remote base");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Fresh remote base"]);
    git(&collaborator, ["push", "origin", "main"]);
    git(&backend, ["fetch", "origin", "main"]);

    // A configured base may be intentionally rewritten. The bundle should
    // mirror the fresh remote-tracking state without moving the local base.
    git(&collaborator, ["reset", "--hard", stale_local.trim()]);
    append_line(&collaborator.join("app.txt"), "rewritten remote base");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Rewrite remote base"]);
    git(&collaborator, ["push", "--force", "origin", "main"]);
    let remote_head = git(&collaborator, ["rev-parse", "HEAD"]);

    // Source checkout state is independent from the bundle base snapshot.
    append_line(&backend.join("app.txt"), "dirty local source checkout");
    knit(&workspace, ["bundle", "fresh feature"]);

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/fresh-feature.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        bundle["repos"][0]["baseSha"].as_str(),
        Some(remote_head.trim())
    );
    assert_eq!(
        git(
            &workspace.join(".knit/worktrees/fresh-feature/backend"),
            ["rev-parse", "HEAD"]
        ),
        remote_head
    );
    assert_eq!(git(&backend, ["rev-parse", "main"]), stale_local);
    assert!(!git(&backend, ["status", "--short"]).trim().is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_base_source_can_be_cached_remote_or_explicit_local_branch() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let local_head = git(&backend, ["rev-parse", "main"]);

    append_line(&collaborator.join("app.txt"), "cached remote base");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Cached remote base"]);
    git(&collaborator, ["push", "origin", "main"]);
    git(&backend, ["fetch", "origin", "main"]);
    let cached_head = git(&backend, ["rev-parse", "origin/main"]);

    append_line(&collaborator.join("app.txt"), "not cached yet");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Uncached remote base"]);
    git(&collaborator, ["push", "origin", "main"]);

    knit(&workspace, ["bundle", "offline feature", "--offline"]);
    assert_eq!(
        git(
            &workspace.join(".knit/worktrees/offline-feature/backend"),
            ["rev-parse", "HEAD"]
        ),
        cached_head
    );

    knit(&workspace, ["bundle", "local feature", "--from-local-base"]);
    assert_eq!(
        git(
            &workspace.join(".knit/worktrees/local-feature/backend"),
            ["rev-parse", "HEAD"]
        ),
        local_head
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn fresh_base_preflight_records_no_repos_or_branches_when_any_fetch_fails() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );
    fs::remove_dir_all(&backend_remote).unwrap();

    let failure = knit_fails(&workspace, ["bundle", "atomic fresh bases"]);
    assert!(
        failure.contains("before any repos were recorded"),
        "{failure}"
    );
    assert!(!workspace
        .join(".knit/bundles/atomic-fresh-bases.bundle.json")
        .exists());
    assert!(
        git(&backend, ["branch", "--list", "knit/atomic-fresh-bases"])
            .trim()
            .is_empty()
    );
    assert!(
        git(&frontend, ["branch", "--list", "knit/atomic-fresh-bases"])
            .trim()
            .is_empty()
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_project_bundle_start_preserves_fallback_and_can_resume() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(
        &workspace,
        [
            "bundle",
            "previous work",
            "--repo",
            "backend",
            "--no-worktree",
        ],
    );

    // A non-worktree directory at the generated target deterministically
    // stands in for a checkout interrupted by ENOSPC or another I/O failure.
    let blocker = workspace.join(".knit/worktrees/failed-start/backend");
    fs::create_dir_all(&blocker).unwrap();
    fs::write(blocker.join("partial.txt"), "partial checkout\n").unwrap();

    let failure = knit_fails(&workspace, ["bundle", "failed start", "--repo", "backend"]);
    assert!(failure.contains("setup did not complete"), "{failure}");
    assert!(
        failure.contains("workspace fallback was not switched"),
        "{failure}"
    );
    assert!(
        failure.contains("knit --bundle failed-start bundle worktree"),
        "{failure}"
    );

    let config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    assert_eq!(config["activeBundle"].as_str(), Some("previous-work"));
    assert_eq!(bundle_repo_ids(&workspace, "failed-start"), vec!["backend"]);

    let failed: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/failed-start.bundle.json")).unwrap(),
    )
    .unwrap();
    assert!(failed["repos"][0]["worktreePath"].is_null());

    fs::remove_dir_all(&blocker).unwrap();
    knit(
        &workspace,
        ["--bundle", "failed-start", "bundle", "worktree"],
    );
    assert!(blocker.join("app.txt").exists());

    let recovered: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/failed-start.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        recovered["repos"][0]["worktreePath"].as_str(),
        Some(".knit/worktrees/failed-start/backend")
    );

    let empty_failure = knit_fails(
        &workspace,
        ["bundle", "empty failure", "--repo", "missing-repo"],
    );
    assert!(
        empty_failure.contains("before any repos were recorded"),
        "{empty_failure}"
    );
    assert!(
        empty_failure.contains("artifact was removed"),
        "{empty_failure}"
    );
    assert!(!workspace
        .join(".knit/bundles/empty-failure.bundle.json")
        .exists());

    let config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    assert_eq!(config["activeBundle"].as_str(), Some("previous-work"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn views_apply_default_and_named_shapes_on_bundle_start() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(
        &workspace,
        ["view", "save", "backend", "--exclude", "frontend"],
    );
    knit(
        &workspace,
        [
            "view",
            "save",
            "frontend",
            "--include",
            "docs",
            "--exclude",
            "backend",
        ],
    );
    knit(&workspace, ["view", "default", "backend"]);

    let list = knit(&workspace, ["view", "list"]);
    assert!(list.contains("backend"), "{list}");
    assert!(list.contains("frontend"), "{list}");

    // Default view (backend) drops frontend; docs is observed, so backend only.
    knit(&workspace, ["bundle", "default feature"]);
    assert_eq!(
        bundle_repo_ids(&workspace, "default-feature"),
        vec!["backend"]
    );

    // Named view "frontend" => {frontend, docs}; ad-hoc --include adds backend.
    knit(
        &workspace,
        [
            "bundle",
            "named feature",
            "--view",
            "frontend",
            "--include",
            "backend",
        ],
    );
    let ids = bundle_repo_ids(&workspace, "named-feature");
    assert_eq!(ids, vec!["backend", "frontend", "docs"], "{ids:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn view_save_accepts_comma_separated_exclude_list() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(
        &workspace,
        ["view", "save", "backend", "--exclude", "frontend,docs"],
    );

    let repos = knit(&workspace, ["view", "show", "backend", "--repos"]);
    assert_eq!(
        repos.lines().collect::<Vec<_>>(),
        vec!["backend"],
        "{repos}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn view_flag_conflicts_with_repo_selection() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    knit(
        &workspace,
        ["view", "save", "backend", "--exclude", "frontend"],
    );

    let error = knit_fails(
        &workspace,
        ["bundle", "x", "--view", "backend", "--repo", "backend"],
    );
    assert!(error.contains("not together with --repo"), "{error}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_include_and_exclude_manage_worktrees() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(&workspace, ["bundle", "live feature"]);
    let worktrees = workspace.join(".knit/worktrees/live-feature");
    assert!(worktrees.join("backend").exists());
    assert!(worktrees.join("frontend").exists());
    assert!(!worktrees.join("docs").exists());

    // Include the observed repo: its worktree is materialized.
    knit(&workspace, ["bundle", "add", "docs"]);
    assert!(worktrees.join("docs").exists());
    assert!(bundle_repo_ids(&workspace, "live-feature").contains(&"docs".to_string()));

    // Exclude (default): worktree removed, feature branch kept.
    knit(&workspace, ["bundle", "remove", "frontend"]);
    assert!(!worktrees.join("frontend").exists());
    assert!(!bundle_repo_ids(&workspace, "live-feature").contains(&"frontend".to_string()));
    assert!(
        git(
            &root.join("frontend"),
            ["branch", "--list", "knit/live-feature"]
        )
        .contains("knit/live-feature"),
        "feature branch should be preserved by default"
    );

    // Exclude with --delete-branch: worktree removed and branch deleted.
    knit(&workspace, ["bundle", "remove", "docs", "--delete-branch"]);
    assert!(!worktrees.join("docs").exists());
    assert!(
        !git(
            &root.join("docs"),
            ["branch", "--list", "knit/live-feature"]
        )
        .contains("knit/live-feature"),
        "feature branch should be deleted with --delete-branch"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_remove_and_readd_preserves_committed_diff() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(&workspace, ["bundle", "readd feature", "--repo", "backend"]);
    let checkout = workspace.join(".knit/worktrees/readd-feature/backend");
    append_line(&checkout.join("app.txt"), "preserved bundle work");
    knit(&workspace, ["commit", "--all", "-m", "Preserve this work"]);

    knit(&workspace, ["bundle", "remove", "backend"]);
    assert!(!checkout.exists());
    let readded = knit(&workspace, ["bundle", "add", "backend"]);
    assert!(readded.contains("existing branch"), "{readded}");

    let diff = knit(&workspace, ["diff", "backend"]);
    assert!(diff.contains("preserved bundle work"), "{diff}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_add_rejects_duplicate_instead_of_rewriting_recorded_base() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(&workspace, ["bundle", "duplicate add", "--repo", "backend"]);
    let bundle_path = workspace.join(".knit/bundles/duplicate-add.bundle.json");
    let before: Value = serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();

    let duplicate = knit_fails(&workspace, ["bundle", "add", "backend"]);
    assert!(duplicate.contains("already tracked"), "{duplicate}");
    assert!(
        duplicate.contains("will not rewrite its recorded base"),
        "{duplicate}"
    );

    let after: Value = serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    assert_eq!(after["repos"][0]["baseSha"], before["repos"][0]["baseSha"]);
    assert_eq!(
        after["repos"][0]["baseBranch"],
        before["repos"][0]["baseBranch"]
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_no_diff_explains_bundle_source_and_untracked_state() {
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
    knit(&workspace, ["bundle", "diagnostic diff"]);
    let checkout = workspace.join(".knit/worktrees/diagnostic-diff/backend");

    append_line(&backend.join("app.txt"), "edited in source checkout");
    let source_only = knit(&workspace, ["diff", "backend"]);
    assert!(source_only.contains("no diff"), "{source_only}");
    assert!(source_only.contains("bundle checkout:"), "{source_only}");
    assert!(source_only.contains("source checkout"), "{source_only}");
    assert!(source_only.contains("modified"), "{source_only}");

    fs::write(checkout.join("untracked.txt"), "not tracked\n").unwrap();
    let untracked = knit(&workspace, ["diff", "backend"]);
    assert!(untracked.contains("untracked files"), "{untracked}");
    assert!(untracked.contains("does not include them"), "{untracked}");

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn bundle_start_cd_opens_project_worktree_root() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    let output = knit_with_env(
        &workspace,
        ["bundle", "project feature", "--agents", "--cd"],
        &[("SHELL", "/bin/pwd")],
    );
    let checkout = workspace
        .join(".knit/worktrees/project-feature")
        .canonicalize()
        .unwrap();
    assert!(checkout.join("backend").exists());
    assert!(checkout.join("frontend").exists());
    assert!(output.contains("Bundle AGENTS.md:"));
    let bundle_agents = fs::read_to_string(checkout.join("AGENTS.md")).unwrap();
    assert!(bundle_agents.contains("Knit Bundle Worktree Guide"));
    assert!(bundle_agents.contains("bundle `project-feature`"));
    assert!(bundle_agents.contains("knit status"));
    assert!(bundle_agents.contains("`backend`: `.knit/worktrees/project-feature/backend`"));
    assert!(bundle_agents.contains("`frontend`: `.knit/worktrees/project-feature/frontend`"));
    assert!(output.contains("cd:"));
    assert!(
        output
            .lines()
            .any(|line| line.trim() == checkout.to_str().unwrap()),
        "{output}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn bundle_start_cd_accepts_repo_selector() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    let output = knit_with_env(
        &workspace,
        ["bundle", "project feature", "--cd", "frontend"],
        &[("SHELL", "/bin/pwd")],
    );
    let checkout = workspace
        .join(".knit/worktrees/project-feature/frontend")
        .canonicalize()
        .unwrap();
    assert!(
        output
            .lines()
            .any(|line| line.trim() == checkout.to_str().unwrap()),
        "{output}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_remove_deletes_template_and_clears_active_project() {
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

    let project_path = workspace.join(".knit/projects/demo.project.json");
    assert!(project_path.exists());
    let refused = knit_fails(&workspace, ["project", "remove", "demo"]);
    assert!(refused.contains("requires --force"));

    let removed = knit(&workspace, ["project", "remove", "demo", "--force"]);
    assert!(removed.contains("Removed project"));
    assert!(!project_path.exists());
    let config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    assert!(config.get("activeProject").is_none());
    assert!(!knit(&workspace, ["project", "list"]).contains("demo"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_remove_repo_drops_entry_but_keeps_template() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    let project_path = workspace.join(".knit/projects/demo.project.json");
    let removed = knit(
        &workspace,
        ["project", "remove", "demo", "--repo", "frontend"],
    );
    assert!(removed.contains("Removed repo"));
    assert!(removed.contains("frontend"));

    // Template still exists; only the repo entry is gone. The checkout stays.
    assert!(project_path.exists());
    let project: Value = serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    let ids: Vec<&str> = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["backend"]);
    assert!(frontend.exists());

    // Removing an unknown repo is an error.
    let missing = knit_fails(
        &workspace,
        ["project", "remove", "demo", "--repo", "frontend"],
    );
    assert!(missing.contains("no repo"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_remove_repo_refuses_when_open_bundle_tracks_it() {
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
    // An open bundle tracking backend must block its removal.
    knit(&workspace, ["bundle", "feature one", "--repo", "backend"]);

    let refused = knit_fails(
        &workspace,
        ["project", "remove", "demo", "--repo", "backend"],
    );
    assert!(refused.contains("open bundle"), "{refused}");
    assert!(refused.contains("backend"), "{refused}");

    // The repo entry is untouched.
    let project: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/projects/demo.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(project["repos"].as_array().unwrap().len(), 1);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cherrypick_moves_source_commits_into_destination_bundle() {
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
        ["bundle", "source feature", "--repo", "backend"],
    );
    let source_checkout = workspace.join(".knit/worktrees/source-feature/backend");
    append_line(&source_checkout.join("app.txt"), "source change");
    git(&source_checkout, ["add", "app.txt"]);
    git(&source_checkout, ["commit", "-m", "Source change"]);
    let source_sha = git(&source_checkout, ["rev-parse", "HEAD"]);
    knit(&workspace, ["--bundle", "source-feature", "sync"]);

    knit(
        &workspace,
        ["bundle", "picked feature", "--repo", "backend"],
    );
    let picked = knit(
        &workspace,
        [
            "--bundle",
            "picked-feature",
            "cherrypick",
            "--from",
            "source-feature",
            source_sha.trim(),
        ],
    );
    assert!(picked.contains("picking"));

    let picked_checkout = workspace.join(".knit/worktrees/picked-feature/backend");
    assert!(fs::read_to_string(picked_checkout.join("app.txt"))
        .unwrap()
        .contains("source change"));
    assert!(git(&picked_checkout, ["log", "-1", "--pretty=%B"]).contains("Source change"));

    let picked_bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/picked-feature.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(picked_bundle["repos"][0]["id"].as_str(), Some("backend"));
    assert!(picked_bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["type"].as_str() == Some("git.observed")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_agents_are_generated_from_project_json() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    fs::write(
        workspace.join("AGENTS.md"),
        "custom guidance\n\n## Demo Knit Project\n\nThat command should add all four Demo repos by default:\n\n- `backend`\n\n<!-- BEGIN GLOSS AGENTS -->\nkeep this\n<!-- END GLOSS AGENTS -->\n",
    )
    .unwrap();

    let refresh = knit(&workspace, ["init", "demo", "--agents"]);
    assert!(refresh.contains("Project AGENTS.md"));
    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("custom guidance"));
    assert!(agents.contains("<!-- BEGIN KNIT PROJECT AGENTS: demo -->"));
    assert!(agents.contains("That command adds these default repos from the project data:"));
    assert!(agents.contains("- `backend`"));
    assert!(agents.contains("### Agent Teamwork"));
    assert!(agents.contains("minimum capable subagent/model"));
    assert!(!agents.contains("all four Demo repos"));
    assert!(agents.contains("<!-- BEGIN GLOSS AGENTS -->"));
    assert!(agents.contains("<!-- END KNIT PROJECT AGENTS: demo -->\n<!-- BEGIN GLOSS AGENTS -->"));

    knit(
        &workspace,
        [
            "project",
            "add",
            "frontend",
            frontend.to_str().unwrap(),
            "--agents",
        ],
    );
    let updated = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert_eq!(
        updated
            .matches("<!-- BEGIN KNIT PROJECT AGENTS: demo -->")
            .count(),
        1
    );
    assert!(updated.contains("- `backend`"));
    assert!(updated.contains("- `frontend`"));

    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "merge": {
            "repoOrder": ["backend", "frontend"],
            "method": "squash",
            "requiredChecksOnly": true
        },
        "deployments": [
            {
                "id": "deploy-backend",
                "repoId": "backend",
                "checkout": { "branch": "main", "remote": "origin", "update": "pull" },
                "timeoutSeconds": 900,
                "command": ["fly", "deploy"]
            },
            {
                "id": "deploy-frontend",
                "repoId": "frontend",
                "mode": "push"
            }
        ],
        "targets": {
            "staging": {
                "deployments": [{
                    "id": "deploy-staging",
                    "repoId": "backend",
                    "checkout": { "branch": "staging", "remote": "origin", "update": "pull" },
                    "command": ["fly", "deploy", "--config", "fly.staging.toml"]
                }]
            }
        }
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["project", "agents", "demo"]);
    let landing_agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(landing_agents.contains("This project defines a default landing template"));
    assert!(landing_agents
        .contains("`knit land` expands it into `.knit/land-plans/<bundle>.land.json`"));
    assert!(landing_agents.contains("- `backend`"));
    assert!(landing_agents.contains("- `frontend`"));
    assert!(
        landing_agents.contains("Merge defaults: method `squash`, required checks only `true`.")
    );
    assert!(landing_agents.contains("`deploy-backend` repo `backend` uses `command` from `origin/main` with `pull`: `fly deploy`"));
    assert!(landing_agents.contains("timeout: 900s"));
    assert!(landing_agents.contains("`deploy-frontend` repo `frontend` uses `push`"));
    assert!(landing_agents.contains("Configured branch-target deployment steps"));
    assert!(landing_agents.contains("- `staging`:"));
    assert!(landing_agents.contains("`deploy-staging` repo `backend` uses `command` from `origin/staging` with `pull`: `fly deploy --config fly.staging.toml`"));
    assert!(landing_agents.contains("Do not use `gh pr merge` for Knit-owned bundles."));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_agents_are_written_at_bundle_root_not_in_repo_checkouts() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    // A repo that tracks its own AGENTS.md must keep it byte-identical in the
    // checkout: a Knit-written section would become a tracked modification,
    // get committed with the bundle, and conflict on every publish.
    fs::write(backend.join("AGENTS.md"), "backend guidance\n").unwrap();
    git(&backend, ["add", "AGENTS.md"]);
    git(&backend, ["commit", "-m", "Add backend agents guidance"]);
    init_repo(&frontend, "frontend");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    let start = knit(&workspace, ["bundle", "agent docs"]);
    assert!(!start.contains("Worktree AGENTS.md"));
    assert!(!workspace.join("AGENTS.md").exists());

    let bundle_agents_path = workspace.join(".knit/worktrees/agent-docs/AGENTS.md");
    let backend_agents_path = workspace.join(".knit/worktrees/agent-docs/backend/AGENTS.md");
    let frontend_agents_path = workspace.join(".knit/worktrees/agent-docs/frontend/AGENTS.md");
    let bundle_agents = fs::read_to_string(&bundle_agents_path).unwrap();
    assert!(bundle_agents.contains("Knit Bundle Worktree Guide"));
    assert!(bundle_agents.contains("bundle `agent-docs`"));
    assert!(bundle_agents.contains("`backend`: `.knit/worktrees/agent-docs/backend`"));
    assert!(bundle_agents.contains("`frontend`: `.knit/worktrees/agent-docs/frontend`"));
    assert!(bundle_agents.contains("## Agent Teamwork"));
    assert!(bundle_agents.contains("minimum capable subagent/model"));
    assert!(bundle_agents.contains("knit commit --all"));
    assert!(bundle_agents.contains("knit push --set-upstream"));
    assert!(!bundle_agents.contains("knit --bundle"));
    assert_eq!(
        fs::read_to_string(&backend_agents_path).unwrap(),
        "backend guidance\n"
    );
    assert!(!frontend_agents_path.exists());
    assert!(git(
        &workspace.join(".knit/worktrees/agent-docs/backend"),
        ["status", "--short"]
    )
    .trim()
    .is_empty());

    knit(&workspace, ["bundle", "agent docs", "--agents"]);
    let refreshed = fs::read_to_string(&bundle_agents_path).unwrap();
    assert_eq!(refreshed.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);
    assert_eq!(
        fs::read_to_string(&backend_agents_path).unwrap(),
        "backend guidance\n"
    );
    assert!(workspace.join("AGENTS.md").exists());

    let second_start = knit(&workspace, ["bundle", "agent docs two"]);
    assert!(!second_start.contains("Worktree AGENTS.md"));
    let second_agents =
        fs::read_to_string(workspace.join(".knit/worktrees/agent-docs-two/AGENTS.md")).unwrap();
    assert!(second_agents.contains("bundle `agent-docs-two`"));
    assert!(!workspace
        .join(".knit/worktrees/agent-docs-two/frontend/AGENTS.md")
        .exists());

    fs::remove_dir_all(root).unwrap();
}
