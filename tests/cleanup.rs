mod common;

use common::*;
use serde_json::Value;
use std::fs;

#[test]
fn archive_records_ledger_node_and_preserves_branches() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    let head_before_archive = git(&feature, ["rev-parse", "HEAD"]);

    // A dirty generated worktree blocks archive unless --force discards it.
    append_line(&feature.join("app.txt"), "uncommitted work");
    let dirty = knit_fails(&workspace, ["bundle", "archive", "venue-capacity"]);
    assert!(dirty.contains("--force"));
    git(&feature, ["checkout", "--", "app.txt"]);

    let archive = knit(
        &workspace,
        [
            "bundle",
            "archive",
            "venue-capacity",
            "--reason",
            "merged",
            "--keep-worktrees",
        ],
    );
    assert!(archive.contains("Archived bundle"));
    assert!(archive.contains("Preserved"));
    assert!(feature.exists());

    let bundle = read_bundle(&workspace);
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("feature.archived"));
    assert_eq!(latest["message"].as_str(), Some("merged"));
    assert_eq!(
        bundle["headNodeId"].as_str(),
        Some(latest["id"].as_str().unwrap())
    );
    assert_eq!(git(&feature, ["rev-parse", "HEAD"]), head_before_archive);
    assert!(
        git(&backend, ["branch", "--list", "knit/venue-capacity"]).contains("knit/venue-capacity")
    );

    // Worktrees kept by --keep-worktrees are cleaned up by `clean --archived`.
    knit(&workspace, ["clean", "--archived", "--worktrees"]);
    assert!(!feature.exists());

    let second_archive = knit_fails(&workspace, ["bundle", "archive", "venue-capacity"]);
    assert!(second_archive.contains("already archived"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn clean_removes_plans_and_generated_worktrees_only() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let worktree = workspace.join(".knit/worktrees/venue-capacity/backend");

    append_line(&worktree.join("app.txt"), "clean test change");
    knit(&workspace, ["commit", "--all", "-m", "Clean test change"]);
    knit(&workspace, ["revert", "HEAD"]);
    assert!(workspace.join(".knit/revert-plans").exists());

    let no_target = knit_fails(&workspace, ["clean"]);
    assert!(no_target.contains("Choose what to clean"));

    let clean_plans = knit(&workspace, ["clean", "--plans"]);
    assert!(clean_plans.contains("removed"));
    assert!(!workspace.join(".knit/revert-plans").exists());

    let clean_worktrees = knit(&workspace, ["clean", "--worktrees"]);
    assert!(clean_worktrees.contains("backend"));
    assert!(clean_worktrees.contains("removed"));
    assert!(!worktree.exists());
    assert!(backend.exists());
    assert!(
        git(&backend, ["branch", "--list", "knit/venue-capacity"]).contains("knit/venue-capacity")
    );

    let bundle = read_bundle(&workspace);
    assert!(bundle["repos"][0]["worktreePath"].is_null());
    let valid = knit(&workspace, ["bundle", "validate"]);
    assert!(valid.contains("Bundle valid"));
    let status_after_clean = knit(&workspace, ["status"]);
    assert!(status_after_clean.contains("(not materialized)"));
    assert!(status_after_clean.contains("missing worktree"));
    let git_after_clean = knit_fails(&workspace, ["git", "status", "--short", "backend"]);
    assert!(git_after_clean.contains("has no active checkout"));

    knit(&workspace, ["bundle", "worktree"]);
    assert!(worktree.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_delete_can_remove_generated_worktrees_and_force_delete_branches() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "throwaway"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let worktree = workspace.join(".knit/worktrees/throwaway/backend");
    append_line(&worktree.join("app.txt"), "throwaway change");
    knit(&workspace, ["commit", "--all", "-m", "Throwaway change"]);
    assert!(git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));

    let safe_delete = knit_fails(
        &workspace,
        [
            "bundle",
            "delete",
            "throwaway",
            "--force",
            "--worktrees",
            "--branches",
        ],
    );
    assert!(safe_delete.contains("failed to delete feature branches"));
    assert!(workspace
        .join(".knit/bundles/throwaway.bundle.json")
        .exists());
    assert!(!worktree.exists());
    assert!(git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));

    let forced_delete = knit(
        &workspace,
        [
            "bundle",
            "delete",
            "throwaway",
            "--force",
            "--worktrees",
            "--branches",
            "--force-branches",
        ],
    );
    assert!(forced_delete.contains("Deleted bundle"));
    assert!(!workspace
        .join(".knit/bundles/throwaway.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/throwaway.bundle.json")
        .exists());
    assert!(!git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));
    let deleted: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/deleted/bundles/throwaway.bundle.json")).unwrap(),
    )
    .unwrap();
    assert!(deleted["repos"][0]["worktreePath"].is_null());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn delete_recovers_generated_worktree_when_recorded_path_was_lost() {
    // A bundle synced back from a remote is localized, which clears the
    // local-only worktreePath even though the generated checkout still exists
    // and holds its feature branch. Cleanup must fall back to the conventional
    // location so it removes the worktree and frees the branch for deletion.
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "throwaway"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let worktree = workspace.join(".knit/worktrees/throwaway/backend");
    append_line(&worktree.join("app.txt"), "throwaway change");
    knit(&workspace, ["commit", "--all", "-m", "Throwaway change"]);

    // Simulate the post-localize state: the recorded worktree path is gone.
    let bundle_path = workspace.join(".knit/bundles/throwaway.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    bundle["repos"][0]["worktreePath"] = Value::Null;
    fs::write(&bundle_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();
    assert!(worktree.exists());
    assert!(git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));

    let deleted = knit(
        &workspace,
        [
            "bundle",
            "delete",
            "throwaway",
            "--force",
            "--worktrees",
            "--branches",
            "--force-branches",
        ],
    );
    assert!(deleted.contains("Deleted bundle"));
    assert!(!worktree.exists());
    assert!(!git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));
    assert!(!bundle_path.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_prune_removes_only_bundles_with_all_recorded_prs_merged() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["bundle", "merged cleanup"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    write_bundle_publications(&workspace, "merged-cleanup", "MERGED");

    knit(&workspace, ["bundle", "open cleanup"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    write_bundle_publications(&workspace, "open-cleanup", "OPEN");

    let preview = knit(&workspace, ["bundle", "prune", "--no-refresh"]);
    assert!(preview.contains("Dead bundle candidates"));
    assert!(preview.contains("merged-cleanup"));
    assert!(!preview.contains("open-cleanup"));
    assert!(workspace
        .join(".knit/bundles/merged-cleanup.bundle.json")
        .exists());

    let pruned = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--apply",
            "--worktrees",
            "--branches",
        ],
    );
    assert!(pruned.contains("Deleted bundle"));
    assert!(pruned.contains("Pruned"));
    assert!(!workspace
        .join(".knit/bundles/merged-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/merged-cleanup.bundle.json")
        .exists());
    assert!(!workspace
        .join(".knit/worktrees/merged-cleanup/backend")
        .exists());
    assert!(
        !git(&backend, ["branch", "--list", "knit/merged-cleanup"]).contains("knit/merged-cleanup")
    );

    assert!(workspace
        .join(".knit/bundles/open-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/worktrees/open-cleanup/backend")
        .exists());
    assert!(git(&backend, ["branch", "--list", "knit/open-cleanup"]).contains("knit/open-cleanup"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_prune_refreshes_stale_publication_states_by_default() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    write_bundle_publications(&workspace, "venue-capacity", "OPEN");

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    fs::write(fake_gh_dir.join("merged-backend"), "").unwrap();
    fs::write(fake_gh_dir.join("merged-frontend"), "").unwrap();

    let preview = knit_with_fake_gh(&workspace, ["bundle", "prune"], &fake_bin, &fake_gh_dir);
    assert!(preview.contains("Dead bundle candidates"));
    assert!(preview.contains("venue-capacity"));

    let bundle = read_bundle(&workspace);
    assert!(bundle["publications"]
        .as_array()
        .unwrap()
        .iter()
        .all(|publication| publication["state"].as_str() == Some("MERGED")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_prune_removes_clean_dead_work_with_missing_publications() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["bundle", "partial landed"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    write_bundle_publications_for_repos(&workspace, "partial-landed", "MERGED", &["backend"]);

    knit(&workspace, ["bundle", "abandoned cleanup"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    knit(&workspace, ["bundle", "dirty cleanup"]);
    knit(&workspace, ["bundle", "add", frontend.to_str().unwrap()]);
    let dirty_feature = workspace.join(".knit/worktrees/dirty-cleanup/frontend");
    append_line(&dirty_feature.join("app.txt"), "dirty local edit");

    let preview = knit(
        &workspace,
        ["bundle", "prune", "--no-refresh", "--worktrees"],
    );
    assert!(preview.contains("partial-landed"));
    assert!(preview.contains("recorded PRs are merged"));
    assert!(preview.contains("abandoned-cleanup"));
    assert!(preview.contains("no recorded PRs and no pending changes"));
    assert!(!preview.contains("dirty-cleanup"));

    let pruned = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--apply",
            "--worktrees",
            "--branches",
            "--force-branches",
        ],
    );
    assert!(pruned.contains("partial-landed"));
    assert!(pruned.contains("abandoned-cleanup"));
    assert!(!workspace
        .join(".knit/bundles/partial-landed.bundle.json")
        .exists());
    assert!(!workspace
        .join(".knit/bundles/abandoned-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/partial-landed.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/abandoned-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/bundles/dirty-cleanup.bundle.json")
        .exists());
    assert!(dirty_feature.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_prune_untracked_flag_prunes_untracked_only_dead_work() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    // Dead work whose only uncommitted content is an untracked file.
    knit(&workspace, ["bundle", "stray cleanup"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let stray_feature = workspace.join(".knit/worktrees/stray-cleanup/backend");
    fs::write(stray_feature.join("PROOF.md"), "untracked proof\n").unwrap();

    // Dead work with a tracked modification must stay protected even with --untracked.
    knit(&workspace, ["bundle", "dirty cleanup"]);
    knit(&workspace, ["bundle", "add", frontend.to_str().unwrap()]);
    let dirty_feature = workspace.join(".knit/worktrees/dirty-cleanup/frontend");
    append_line(&dirty_feature.join("app.txt"), "dirty local edit");

    // Default prune surfaces the untracked-only bundle separately and does not
    // treat it as a deletable candidate.
    let preview = knit(&workspace, ["bundle", "prune", "--no-refresh"]);
    assert!(preview.contains("Blocked by untracked files"));
    assert!(preview.contains("stray-cleanup"));
    assert!(!preview.contains("Dead bundle candidates"));

    // --report names every bundle and its status, including kept ones.
    let report = knit(&workspace, ["bundle", "prune", "--no-refresh", "--report"]);
    assert!(report.contains("Bundle report:"));
    assert!(report.contains("prunable with --untracked"));
    assert!(report.contains("dirty-cleanup"));
    assert!(report.contains("uncommitted tracked changes"));

    // --untracked promotes the untracked-only bundle to a real candidate while
    // still protecting the tracked-change bundle.
    let untracked_preview = knit(
        &workspace,
        ["bundle", "prune", "--no-refresh", "--untracked"],
    );
    assert!(untracked_preview.contains("Dead bundle candidates"));
    assert!(untracked_preview.contains("stray-cleanup"));
    assert!(untracked_preview.contains("discards untracked files"));
    assert!(!untracked_preview.contains("dirty-cleanup"));

    let pruned = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--untracked",
            "--apply",
            "--worktrees",
            "--branches",
            "--force-branches",
        ],
    );
    assert!(pruned.contains("stray-cleanup"));
    assert!(!workspace
        .join(".knit/bundles/stray-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/stray-cleanup.bundle.json")
        .exists());
    assert!(!stray_feature.exists());

    // The tracked-change bundle and its worktree survive.
    assert!(workspace
        .join(".knit/bundles/dirty-cleanup.bundle.json")
        .exists());
    assert!(dirty_feature.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn prune_removes_orphan_worktree_dirs_without_bundle_artifacts() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "dirty active"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let active_feature = workspace.join(".knit/worktrees/dirty-active/backend");
    append_line(&active_feature.join("app.txt"), "keep me");

    let empty_orphan = workspace.join(".knit/worktrees/empty-orphan/nested/leaf");
    fs::create_dir_all(&empty_orphan).unwrap();
    let dirty_orphan = workspace.join(".knit/worktrees/dirty-orphan");
    fs::create_dir_all(&dirty_orphan).unwrap();
    fs::write(dirty_orphan.join("note.txt"), "untracked work\n").unwrap();

    let preview = knit(
        &workspace,
        ["bundle", "prune", "--no-refresh", "--worktrees"],
    );
    assert!(preview.contains("Orphan worktree candidates"));
    assert!(preview.contains("empty-orphan"));
    assert!(preview.contains("Blocked orphan worktrees"));
    assert!(preview.contains("dirty-orphan"));
    assert!(preview.contains("--force"));
    assert!(!preview.contains("dirty-active"));

    let pruned = knit(
        &workspace,
        ["bundle", "prune", "--no-refresh", "--apply", "--worktrees"],
    );
    assert!(pruned.contains("removed orphan worktree"));
    assert!(!workspace.join(".knit/worktrees/empty-orphan").exists());
    assert!(dirty_orphan.exists());
    assert!(active_feature.exists());

    let forced = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--apply",
            "--worktrees",
            "--force",
        ],
    );
    assert!(forced.contains("removed orphan worktree"));
    assert!(!dirty_orphan.exists());
    assert!(active_feature.exists());
    assert!(workspace
        .join(".knit/bundles/dirty-active.bundle.json")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn prune_can_remove_generated_worktrees_local_branches_and_remote_branches() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "remote cleanup"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/remote-cleanup/backend");
    append_line(&feature.join("app.txt"), "remote cleanup change");
    knit(
        &workspace,
        ["commit", "--all", "-m", "Remote cleanup change"],
    );
    git(&feature, ["push", "origin", "knit/remote-cleanup"]);
    assert!(git_success(
        &backend_remote,
        ["rev-parse", "--verify", "refs/heads/knit/remote-cleanup"],
    ));
    assert!(git_success(
        &backend,
        [
            "rev-parse",
            "--verify",
            "refs/remotes/origin/knit/remote-cleanup",
        ],
    ));

    write_bundle_publications(&workspace, "remote-cleanup", "MERGED");
    let preview = knit(&workspace, ["bundle", "prune", "--no-refresh", "--all"]);
    assert!(preview.contains("knit bundle prune --apply --all"));

    let pruned = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--apply",
            "--worktrees",
            "--branches",
            "--force-branches",
            "--remote-branches",
        ],
    );
    assert!(pruned.contains("Deleted bundle"));
    assert!(pruned.contains("origin/knit/remote-cleanup"));
    assert!(!feature.exists());
    assert!(!git_success(
        &backend,
        ["rev-parse", "--verify", "refs/heads/knit/remote-cleanup"],
    ));
    assert!(!git_success(
        &backend_remote,
        ["rev-parse", "--verify", "refs/heads/knit/remote-cleanup"],
    ));
    assert!(!git_success(
        &backend,
        [
            "rev-parse",
            "--verify",
            "refs/remotes/origin/knit/remote-cleanup",
        ],
    ));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn prune_removes_bundle_worktree_container_dir_and_agents_md() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "container cleanup"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    let bundle_root = workspace.join(".knit/worktrees/container-cleanup");
    let feature = bundle_root.join("backend");
    assert!(feature.exists());
    assert!(bundle_root.join("AGENTS.md").exists());

    let pruned = knit(
        &workspace,
        ["bundle", "prune", "--no-refresh", "--apply", "--worktrees"],
    );
    assert!(pruned.contains("Deleted bundle"));
    assert!(pruned.contains("container-cleanup"));
    assert!(!feature.exists());
    assert!(!bundle_root.join("AGENTS.md").exists());
    assert!(!bundle_root.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_prune_keeps_bundles_with_unpublished_commits() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");

    // Committed (but never published) local work: clean checkout, no PRs.
    knit(&workspace, ["bundle", "quiet local work"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/quiet-local-work/backend");
    append_line(&feature.join("app.txt"), "committed but unpublished");
    knit(&workspace, ["commit", "--all", "-m", "Unpublished work"]);

    // A second bundle whose only commits live on origin: another user pushed
    // the feature branch, this workspace just tracks the bundle.
    git(&collaborator, ["checkout", "-b", "knit/remote-only-work"]);
    append_line(&collaborator.join("app.txt"), "remote unpublished work");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote unpublished work"]);
    git(&collaborator, ["push", "origin", "knit/remote-only-work"]);
    knit(&workspace, ["bundle", "remote only work"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    let report = knit(
        &workspace,
        ["bundle", "prune", "--no-refresh", "--report", "--worktrees"],
    );
    assert!(report.contains("quiet-local-work"));
    assert!(report.contains("unpublished commits"));

    let pruned = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--apply",
            "--worktrees",
            "--branches",
            "--force-branches",
        ],
    );
    assert!(!pruned.contains("deleted quiet-local-work"));
    assert!(!pruned.contains("deleted remote-only-work"));
    assert!(workspace
        .join(".knit/bundles/quiet-local-work.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/bundles/remote-only-work.bundle.json")
        .exists());

    fs::remove_dir_all(root).unwrap();
}
