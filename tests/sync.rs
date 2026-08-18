mod common;

use common::*;
use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn pull_updates_original_base_checkout_and_bundle_base_sha() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature_head_before = git(
        &workspace.join(".knit/worktrees/venue-capacity/backend"),
        ["rev-parse", "HEAD"],
    );

    append_line(&collaborator.join("app.txt"), "remote base update");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote base update"]);
    git(&collaborator, ["push", "origin", "main"]);
    let remote_sha = git(&collaborator, ["rev-parse", "HEAD"]);

    let pull = knit(&workspace, ["pull", "backend"]);
    assert!(pull.contains("backend"));
    assert!(pull.contains(&remote_sha[..7]));
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), remote_sha);

    let bundle = read_bundle(&workspace);
    assert_eq!(
        bundle["repos"][0]["baseSha"].as_str(),
        Some(remote_sha.trim())
    );
    assert_eq!(
        git(
            &workspace.join(".knit/worktrees/venue-capacity/backend"),
            ["rev-parse", "HEAD"],
        ),
        feature_head_before
    );

    append_line(&collaborator.join("app.txt"), "second remote base update");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Second remote base update"]);
    git(&collaborator, ["push", "origin", "main"]);
    append_line(&backend.join("app.txt"), "local dirty base checkout");

    let dirty_pull = knit_fails(&workspace, ["pull", "backend"]);
    assert!(dirty_pull.contains("Refusing to pull with uncommitted changes"));
    assert!(dirty_pull.contains("backend"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_current_fast_forwards_project_repos_and_reports() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, backend_collab) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collab) = init_remote_repo(&root, "frontend");
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

    // Advance backend's origin main; leave frontend with a local dirty edit.
    append_line(&backend_collab.join("app.txt"), "remote main update");
    git(&backend_collab, ["add", "app.txt"]);
    git(&backend_collab, ["commit", "-m", "Remote main update"]);
    git(&backend_collab, ["push", "origin", "main"]);
    let backend_sha = git(&backend_collab, ["rev-parse", "HEAD"]);
    append_line(&frontend.join("app.txt"), "local uncommitted edit");

    let report = knit(&workspace, ["pull", "--current"]);
    assert!(report.contains("Current checkouts:"));
    assert!(report.contains("backend"));
    assert!(report.contains(&backend_sha[..7]));
    assert!(report.contains("frontend"));
    assert!(report.contains("skipped"));

    // Backend's source checkout fast-forwarded; the dirty repo was left alone.
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), backend_sha);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_base_updates_configured_base_without_switching_current_checkout() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    git(&backend, ["checkout", "-b", "topic"]);
    let topic_head = git(&backend, ["rev-parse", "HEAD"]);

    append_line(&collaborator.join("app.txt"), "remote base update");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote base update"]);
    git(&collaborator, ["push", "origin", "main"]);
    let remote_head = git(&collaborator, ["rev-parse", "HEAD"]);

    let report = knit(&workspace, ["pull", "--base"]);
    assert!(report.contains("Base branches:"), "{report}");
    assert!(report.contains("backend"), "{report}");
    assert_eq!(git(&backend, ["branch", "--show-current"]).trim(), "topic");
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), topic_head);
    assert_eq!(git(&backend, ["rev-parse", "main"]), remote_head);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_base_refuses_to_discard_divergent_local_base_commits() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    append_line(&backend.join("app.txt"), "local base commit");
    git(&backend, ["add", "app.txt"]);
    git(&backend, ["commit", "-m", "Local base commit"]);
    let local_base = git(&backend, ["rev-parse", "main"]);
    git(&backend, ["checkout", "-b", "topic"]);
    let topic_head = git(&backend, ["rev-parse", "HEAD"]);

    append_line(&collaborator.join("app.txt"), "remote base commit");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote base commit"]);
    git(&collaborator, ["push", "origin", "main"]);
    let remote_base = git(&collaborator, ["rev-parse", "HEAD"]);

    let report = knit_fails(&workspace, ["pull", "--base"]);
    assert!(report.contains("Base branches:"), "{report}");
    assert!(report.contains("has diverged"), "{report}");
    assert_eq!(git(&backend, ["rev-parse", "main"]), local_base);
    assert_eq!(git(&backend, ["rev-parse", "origin/main"]), remote_base);
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), topic_head);
    assert_eq!(git(&backend, ["branch", "--show-current"]).trim(), "topic");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_status_distinguishes_current_checkout_from_configured_base() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    git(&backend, ["checkout", "-b", "topic"]);
    append_line(&backend.join("app.txt"), "dirty topic checkout");

    append_line(&collaborator.join("app.txt"), "remote base update");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote base update"]);
    git(&collaborator, ["push", "origin", "main"]);
    git(&backend, ["fetch", "origin", "main"]);

    let status = knit(&workspace, ["workspace", "status"]);
    assert!(status.contains("Project: demo"), "{status}");
    assert!(status.contains("backend"), "{status}");
    assert!(status.contains("current=topic"), "{status}");
    assert!(status.contains("base=main"), "{status}");
    assert!(status.contains("behind=1"), "{status}");
    assert!(status.contains("dirty"), "{status}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_everything_at_root_reports_without_refusing_multiple_bundles() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collab) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    // Two open bundles: the old workspace-fallback guard refused a bare pull at
    // the root. The new aggregate pull reports instead.
    knit(&workspace, ["bundle", "feature one"]);
    knit(&workspace, ["bundle", "feature two"]);

    let report = knit(&workspace, ["pull"]);
    assert!(!report.contains("Refusing"));
    assert!(report.contains("Current checkouts:"));
    assert!(report.contains("Bundles:"));
    assert!(report.contains("feature-one"));
    assert!(report.contains("feature-two"));
    assert!(report.contains("Pulled:"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_bundles_without_remote_reports_each_bundle_skipped() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "feature one"]);
    knit(&workspace, ["bundle", "feature two"]);

    let report = knit(&workspace, ["pull", "--bundles"]);
    assert!(report.contains("Bundles:"));
    assert!(report.contains("feature-one"));
    assert!(report.contains("feature-two"));
    assert!(report.contains("no sync remote available"));
    // --bundles alone does not touch project main repos.
    assert!(!report.contains("Current checkouts:"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn fetch_updates_remote_refs_without_moving_checkout_or_bundle_base() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let initial_head = git(&backend, ["rev-parse", "HEAD"]);
    let initial_bundle = read_bundle(&workspace);
    let initial_base_sha = initial_bundle["repos"][0]["baseSha"]
        .as_str()
        .unwrap()
        .to_string();

    append_line(&collaborator.join("app.txt"), "remote base fetch");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote base fetch"]);
    git(&collaborator, ["push", "origin", "main"]);
    let remote_sha = git(&collaborator, ["rev-parse", "HEAD"]);

    let fetch = knit(&workspace, ["fetch", "backend"]);
    assert!(fetch.contains("backend"));
    assert!(fetch.contains("origin/main"));
    assert!(fetch.contains(&remote_sha[..7]));
    assert_eq!(git(&backend, ["rev-parse", "origin/main"]), remote_sha);
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), initial_head);

    let bundle = read_bundle(&workspace);
    assert_eq!(
        bundle["repos"][0]["baseSha"].as_str(),
        Some(initial_base_sha.as_str())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn push_sends_feature_branch_and_can_set_upstream() {
    let root = unique_temp_dir();
    let (remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");

    append_line(&feature.join("app.txt"), "feature push");
    knit(&workspace, ["commit", "--all", "-m", "Feature push"]);
    let first_sha = git(&feature, ["rev-parse", "HEAD"]);

    let push = knit(&workspace, ["push", "backend"]);
    assert!(push.contains("backend"));
    assert!(push.contains("origin/knit/venue-capacity"));
    assert!(push.contains(&first_sha[..7]));
    assert_eq!(
        git(&remote, ["rev-parse", "refs/heads/knit/venue-capacity"]),
        first_sha
    );

    append_line(&feature.join("app.txt"), "feature push with upstream");
    knit(
        &workspace,
        ["commit", "--all", "-m", "Feature push with upstream"],
    );
    let second_sha = git(&feature, ["rev-parse", "HEAD"]);

    let push_upstream = knit(&workspace, ["push", "--set-upstream", "backend"]);
    assert!(push_upstream.contains("backend"));
    assert!(push_upstream.contains(&second_sha[..7]));
    assert_eq!(
        git(&remote, ["rev-parse", "refs/heads/knit/venue-capacity"]),
        second_sha
    );
    assert_eq!(
        git(
            &feature,
            ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .trim(),
        "origin/knit/venue-capacity"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn push_force_with_lease_updates_rewritten_feature_branch() {
    let root = unique_temp_dir();
    let (remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");

    append_line(&feature.join("app.txt"), "feature push");
    knit(&workspace, ["commit", "--all", "-m", "Feature push"]);
    knit(&workspace, ["push", "--set-upstream", "backend"]);

    // Rewrite the pushed history: a plain push must be rejected, the leased
    // force push must move the remote branch to the new head.
    git(
        &feature,
        ["commit", "--amend", "-m", "Feature push, reworded"],
    );
    let rewritten_sha = git(&feature, ["rev-parse", "HEAD"]);

    let plain = knit_fails(&workspace, ["push", "backend"]);
    assert!(plain.contains("push failed"), "{plain}");

    let forced = knit(&workspace, ["push", "--force-with-lease", "backend"]);
    assert!(forced.contains(&rewritten_sha[..7]), "{forced}");
    assert_eq!(
        git(&remote, ["rev-parse", "refs/heads/knit/venue-capacity"]),
        rewritten_sha
    );

    // The two force flags are mutually exclusive.
    let conflict = knit_fails(
        &workspace,
        ["push", "--force-with-lease", "--force", "backend"],
    );
    assert!(conflict.contains("--force"), "{conflict}");

    // Rewrite again and verify the unconditional flag also works.
    git(
        &feature,
        ["commit", "--amend", "-m", "Feature push, reworded again"],
    );
    let rewritten_again = git(&feature, ["rev-parse", "HEAD"]);
    let forced_unconditional = knit(&workspace, ["push", "--force", "backend"]);
    assert!(
        forced_unconditional.contains(&rewritten_again[..7]),
        "{forced_unconditional}"
    );
    assert_eq!(
        git(&remote, ["rev-parse", "refs/heads/knit/venue-capacity"]),
        rewritten_again
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn push_skips_missing_implicit_sync_remote_after_git_branch_push() {
    let root = unique_temp_dir();
    let (remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "stale remote"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/stale-remote/backend");

    append_line(
        &feature.join("app.txt"),
        "feature push with stale sync remote",
    );
    knit(&workspace, ["commit", "--all", "-m", "Feature push"]);
    let sha = git(&feature, ["rev-parse", "HEAD"]);

    let config_path = workspace.join(".knit/config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    config["syncRemote"] = serde_json::json!("remote");
    config["syncRemotes"] = serde_json::json!(["remote"]);
    fs::write(
        &config_path,
        format!("{}\n", serde_json::to_string_pretty(&config).unwrap()),
    )
    .unwrap();

    let push = knit(&workspace, ["push", "backend"]);
    assert!(push.contains("backend"), "{push}");
    assert!(push.contains("remote sync skipped (remote):"), "{push}");
    assert_eq!(
        git(&remote, ["rev-parse", "refs/heads/knit/stale-remote"]),
        sha
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(unix)]
fn push_sends_selected_feature_branches_in_parallel() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

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
    let backend_feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    let frontend_feature = workspace.join(".knit/worktrees/venue-capacity/frontend");

    append_line(&backend_feature.join("app.txt"), "parallel backend push");
    append_line(&frontend_feature.join("app.txt"), "parallel frontend push");
    knit(&workspace, ["commit", "--all", "-m", "Parallel push"]);
    let backend_sha = git(&backend_feature, ["rev-parse", "HEAD"]);
    let frontend_sha = git(&frontend_feature, ["rev-parse", "HEAD"]);

    let gate = root.join("push-gate");
    install_parallel_push_hook(&backend_feature, &gate, "backend", "frontend");
    install_parallel_push_hook(&frontend_feature, &gate, "frontend", "backend");

    let push = knit(&workspace, ["push", "backend", "frontend"]);
    assert!(push.contains("backend"));
    assert!(push.contains("frontend"));
    assert_eq!(
        git(
            &backend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        backend_sha
    );
    assert_eq!(
        git(
            &frontend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        frontend_sha
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(unix)]
fn commit_stages_and_commits_repos_in_parallel() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

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
    let backend_feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    let frontend_feature = workspace.join(".knit/worktrees/venue-capacity/frontend");

    append_line(&backend_feature.join("app.txt"), "parallel backend commit");
    append_line(
        &frontend_feature.join("app.txt"),
        "parallel frontend commit",
    );

    let gate = root.join("commit-gate");
    install_parallel_gate_hook(&backend_feature, "pre-commit", &gate, "backend", "frontend");
    install_parallel_gate_hook(
        &frontend_feature,
        "pre-commit",
        &gate,
        "frontend",
        "backend",
    );

    let commit = knit(&workspace, ["commit", "--all", "-m", "Parallel commit"]);
    assert!(commit.contains("backend"));
    assert!(commit.contains("frontend"));
    assert!(commit.contains("Recorded commit group"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn commit_records_a_resolved_merge_even_when_the_index_matches_head() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "resolved merge"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/resolved-merge/backend");

    append_line(&feature.join("app.txt"), "same resolved content");
    knit(&workspace, ["commit", "--all", "-m", "Feature content"]);

    append_line(&collaborator.join("app.txt"), "same resolved content");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Base content"]);
    git(&collaborator, ["push", "origin", "main"]);
    git(&feature, ["fetch", "origin"]);
    git(&feature, ["merge", "--no-commit", "--no-ff", "origin/main"]);

    assert!(git(&feature, ["status", "--short"]).is_empty());
    assert!(!git(&feature, ["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_empty());

    let committed = knit(&workspace, ["commit", "-m", "Record resolved merge"]);
    assert!(committed.contains("backend: committed"));
    assert_eq!(
        git(&feature, ["rev-list", "--parents", "-n", "1", "HEAD"])
            .split_whitespace()
            .count(),
        3
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_feature_checkout_records_observed_git_movement() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    git(&feature, ["push", "-u", "origin", "knit/venue-capacity"]);

    git(
        &collaborator,
        ["fetch", "origin", "knit/venue-capacity:knit/venue-capacity"],
    );
    git(&collaborator, ["checkout", "knit/venue-capacity"]);
    append_line(&collaborator.join("app.txt"), "remote feature update");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote feature update"]);
    git(&collaborator, ["push", "origin", "knit/venue-capacity"]);
    let remote_feature_sha = git(&collaborator, ["rev-parse", "HEAD"]);

    let pull = knit(&workspace, ["pull", "--feature", "backend"]);
    assert!(pull.contains("backend"));
    assert!(pull.contains(&remote_feature_sha[..7]));
    assert!(pull.contains("observed 1 unrecorded commit(s)"));
    assert_eq!(git(&feature, ["rev-parse", "HEAD"]), remote_feature_sha);

    let bundle = read_bundle(&workspace);
    assert_eq!(
        bundle["repos"][0]["headSha"].as_str(),
        Some(remote_feature_sha.trim())
    );
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("git.observed"));
    assert_eq!(latest["repoChanges"][0]["repoId"].as_str(), Some("backend"));
    assert_eq!(
        latest["repoChanges"][0]["movement"].as_str(),
        Some("advanced")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn in_place_repos_operate_in_original_checkout_and_guard_branch() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        ["bundle", "add", "--in-place", backend.to_str().unwrap()],
    );

    assert!(!workspace
        .join(".knit/worktrees/venue-capacity/backend")
        .exists());
    assert_eq!(
        git(&backend, ["branch", "--show-current"]).trim(),
        "knit/venue-capacity"
    );

    let bundle = read_bundle(&workspace);
    let repo = &bundle["repos"][0];
    assert_eq!(repo["checkoutMode"].as_str(), Some("inPlace"));
    assert_eq!(repo["worktreePath"].as_str(), repo["path"].as_str());

    append_line(&backend.join("app.txt"), "in-place feature");
    let status = knit(&workspace, ["status"]);
    assert!(status.contains("in-place"));
    assert!(status.contains("modified"));
    let diff = knit(&workspace, ["diff", "--stat", "backend"]);
    assert!(diff.contains("backend"));
    assert!(diff.contains("app.txt"));

    knit(&workspace, ["commit", "--all", "-m", "In-place feature"]);
    assert!(git(&backend, ["log", "-1", "--pretty=%B"]).contains("In-place feature"));

    git(&backend, ["checkout", "main"]);
    let wrong_branch_status = knit(&workspace, ["status"]);
    assert!(wrong_branch_status.contains("wrong branch"));
    let stage_failure = knit_fails(&workspace, ["add"]);
    assert!(stage_failure.contains("expected `knit/venue-capacity`"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn worktree_materialization_tracks_collaborator_pushed_feature_branch() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    // A collaborator already pushed this bundle's feature branch to origin.
    git(&collaborator, ["checkout", "-b", "knit/venue-capacity"]);
    append_line(&collaborator.join("app.txt"), "collaborator feature work");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Collaborator feature work"]);
    git(&collaborator, ["push", "origin", "knit/venue-capacity"]);
    let collaborator_sha = git(&collaborator, ["rev-parse", "HEAD"]);

    // The local clone has not fetched since that push, so materialization must
    // discover the branch itself instead of forking a new one from base.
    knit(&workspace, ["bundle", "venue capacity"]);
    let add = knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    assert!(add.contains("origin/knit/venue-capacity"));

    let worktree = workspace.join(".knit/worktrees/venue-capacity/backend");
    assert_eq!(git(&worktree, ["rev-parse", "HEAD"]), collaborator_sha);
    assert_eq!(
        git(&worktree, ["rev-parse", "--abbrev-ref", "@{u}"]).trim(),
        "origin/knit/venue-capacity"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn in_place_materialization_tracks_collaborator_pushed_feature_branch() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    git(&collaborator, ["checkout", "-b", "knit/venue-capacity"]);
    append_line(&collaborator.join("app.txt"), "collaborator feature work");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Collaborator feature work"]);
    git(&collaborator, ["push", "origin", "knit/venue-capacity"]);
    let collaborator_sha = git(&collaborator, ["rev-parse", "HEAD"]);

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        ["bundle", "add", "--in-place", backend.to_str().unwrap()],
    );

    assert_eq!(
        git(&backend, ["branch", "--show-current"]).trim(),
        "knit/venue-capacity"
    );
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), collaborator_sha);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_merge_unions_diverged_bundle_ledgers() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["bundle", "venue capacity", "--repo", "backend"],
    );

    // This user records local work in the bundle ledger.
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    append_line(&feature.join("app.txt"), "local ledger work");
    knit(&workspace, ["commit", "--all", "-m", "Local ledger work"]);

    // A collaborator pushed their own commit to the shared feature branch.
    git(&collaborator, ["checkout", "-b", "knit/venue-capacity"]);
    append_line(&collaborator.join("app.txt"), "remote ledger work");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote ledger work"]);
    git(&collaborator, ["push", "origin", "knit/venue-capacity"]);
    let collaborator_sha = git(&collaborator, ["rev-parse", "HEAD"]);
    let collaborator_sha = collaborator_sha.trim();

    // Build the remote artifact the collaborator would have pushed: the same
    // ledger prefix, but with this user's commit node replaced by one only the
    // remote records — diverged ledgers.
    let mut remote_payload = read_bundle(&workspace);
    let local_commit_node_id = remote_payload["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["type"] == "commit.group")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut remote_nodes: Vec<serde_json::Value> = remote_payload["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["id"].as_str() != Some(local_commit_node_id.as_str()))
        .cloned()
        .collect();
    remote_nodes.push(serde_json::json!({
        "id": "kg_20990101_remote",
        "type": "commit.group",
        "createdAt": "2099-01-01T00:00:00.000Z",
        "commitGroupId": "kg_20990101_remote",
        "message": "Remote ledger work",
        "commits": [{"repoId": "backend", "sha": collaborator_sha}],
    }));
    remote_payload["nodes"] = serde_json::Value::Array(remote_nodes);
    remote_payload["commitGroups"] = serde_json::json!([{
        "id": "kg_20990101_remote",
        "message": "Remote ledger work",
        "createdAt": "2099-01-01T00:00:00.000Z",
        "commits": [{"repoId": "backend", "sha": collaborator_sha}],
    }]);
    remote_payload["headNodeId"] = serde_json::json!("kg_20990101_remote");
    remote_payload["repos"][0]["headSha"] = serde_json::json!(collaborator_sha);

    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-1",
                "slug": "venue-capacity",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "remotehash123", "payload": remote_payload},
            }],
            "historyEvents": [],
        }
    });
    let base_url = spawn_fake_remote_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    // Without --merge, diverged ledgers are kept local and reported.
    let plain = knit_with_env(&workspace, ["pull"], &env);
    assert!(plain.contains("diverged"));
    assert!(plain.contains("--merge"));

    // With --merge, the union ledger is saved even though the git branches
    // themselves still need a manual merge.
    let merged_run = knit_with_env(&workspace, ["pull", "--merge"], &env);
    assert!(merged_run.contains("merged ledgers"));

    let bundle = read_bundle(&workspace);
    let node_ids: Vec<&str> = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect();
    assert!(node_ids.contains(&local_commit_node_id.as_str()));
    assert!(node_ids.contains(&"kg_20990101_remote"));
    assert_eq!(bundle["commitGroups"].as_array().unwrap().len(), 2);
    assert_eq!(bundle["headNodeId"].as_str(), Some("kg_20990101_remote"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_pull_discovers_remote_bundles_project_wide() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    // Author a bundle with a commit and capture its artifact — the payload
    // another machine would have pushed to remote — then erase it locally as
    // if it had never existed here.
    knit(&workspace, ["bundle", "remote made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/remote-made/backend");
    append_line(&feature.join("app.txt"), "work from another machine");
    knit(&workspace, ["commit", "--all", "-m", "Remote-machine work"]);
    let artifact_path = workspace.join(".knit/bundles/remote-made.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    fs::remove_file(&artifact_path).unwrap();
    fs::remove_dir_all(workspace.join(".knit/worktrees/remote-made")).unwrap();

    // Two other open bundles make the source-root fallback ambiguous — the
    // situation where the old active-bundle-only sync pull broke.
    knit(&workspace, ["bundle", "other work", "--repo", "backend"]);
    knit(&workspace, ["bundle", "third work", "--repo", "backend"]);

    let mut archived_payload = payload.clone();
    archived_payload["id"] = serde_json::json!("old-landed");
    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [
                {
                    "id": "rb-1",
                    "slug": "remote-made",
                    "lifecycleState": "open",
                    "currentArtifact": {"artifactHash": "hash-remote", "payload": payload},
                },
                {
                    "id": "rb-2",
                    "slug": "dead-bundle",
                    "lifecycleState": "deleted",
                    "currentArtifact": null,
                },
                {
                    "id": "rb-3",
                    "slug": "old-landed",
                    "lifecycleState": "archived",
                    "currentArtifact": {"artifactHash": "hash-old", "payload": archived_payload},
                },
            ],
            "historyEvents": [],
        }
    });
    let base_url = spawn_fake_remote_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(
        &workspace,
        ["sync", "pull", "--bundles", "--remote", "hosted"],
        &env,
    );
    assert!(output.contains("fetched"), "{output}");

    // The open remote-only bundle is localized as an artifact; deleted and
    // archived remote records are not — discovery never resurrects the
    // project's dead-work history.
    assert!(artifact_path.exists());
    let list = knit(&workspace, ["bundle", "list"]);
    assert!(list.contains("remote-made"), "{list}");
    assert!(!list.contains("dead-bundle"), "{list}");
    assert!(!list.contains("old-landed"), "{list}");
    assert!(!workspace
        .join(".knit/bundles/old-landed.bundle.json")
        .exists());

    // `knit fetch --mode knit` shares the project-wide path and must also work
    // from the source root while several open bundles exist.
    let fetch_output = knit_with_env(&workspace, ["fetch", "--mode", "knit"], &env);
    assert!(
        fetch_output.contains("up-to-date") || fetch_output.contains("fetched"),
        "{fetch_output}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_pull_does_not_resurrect_locally_deleted_bundles() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    // Author a bundle and capture the artifact the remote still holds — the
    // copy pushed at publish time, before the bundle was landed and pruned
    // here. Nothing pushes terminal state back, so the remote says "open".
    knit(&workspace, ["bundle", "pruned work", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/pruned-work/backend");
    append_line(&feature.join("app.txt"), "work later landed and pruned");
    knit(&workspace, ["commit", "--all", "-m", "Landed work"]);
    let artifact_path = workspace.join(".knit/bundles/pruned-work.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    knit(&workspace, ["bundle", "delete", "pruned-work", "--force"]);
    assert!(!artifact_path.exists());
    assert!(workspace
        .join(".knit/deleted/bundles/pruned-work.bundle.json")
        .exists());

    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-1",
                "slug": "pruned-work",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "hash-stale-open", "payload": payload},
            }],
            "historyEvents": [],
        }
    });
    let base_url = spawn_fake_remote_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(
        &workspace,
        ["sync", "pull", "--bundles", "--remote", "hosted"],
        &env,
    );
    assert!(output.contains("up-to-date"), "{output}");

    // The local delete quarantine is the authority: the stale-open remote
    // record must not come back as an open, worktree-less bundle.
    assert!(!artifact_path.exists());
    let list = knit(&workspace, ["bundle", "list"]);
    assert!(!list.contains("pruned-work"), "{list}");

    fs::remove_dir_all(root).unwrap();
}

/// A collaborator workspace with no local bundle at all (fresh `knit init` +
/// `knit project add`, or every bundle erased) must still be able to run a
/// bare `knit fetch`: the git side falls back to the project's repos and the
/// remote side lists each remote bundle with its repo -> branch mapping.
#[test]
fn fetch_without_resolvable_bundle_falls_back_to_project_and_lists_remote_bundles() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    // Author the bundle another machine would have pushed, then erase every
    // local trace of it: no bundle resolves in this workspace anymore.
    knit(&workspace, ["bundle", "remote made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/remote-made/backend");
    append_line(&feature.join("app.txt"), "work from another machine");
    knit(&workspace, ["commit", "--all", "-m", "Remote-machine work"]);
    knit(&workspace, ["push", "--set-upstream"]);
    let artifact_path = workspace.join(".knit/bundles/remote-made.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    fs::remove_file(&artifact_path).unwrap();
    fs::remove_dir_all(workspace.join(".knit/worktrees/remote-made")).unwrap();
    git(&backend, ["worktree", "prune"]);
    git(&backend, ["branch", "-D", "knit/remote-made"]);

    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-1",
                "slug": "remote-made",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "hash-1", "payload": payload},
            }],
            "historyEvents": [],
        }
    });
    let base_url = spawn_fake_remote_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(&workspace, ["fetch"], &env);
    assert!(output.contains("origin/main"), "{output}");
    assert!(output.contains("backend -> knit/remote-made"), "{output}");
    assert!(output.contains("fetched"), "{output}");
    assert!(artifact_path.exists());

    fs::remove_dir_all(root).unwrap();
}

/// `knit fetch` + `knit switch` + `knit pull` is the cross-machine flow: after
/// fetch localizes a remote bundle's artifact, pointing the workspace at it
/// and pulling must materialize its worktrees from origin — an artifact that
/// is "up to date" is not the same as a usable checkout.
#[test]
fn pull_materializes_the_pointed_at_bundle_after_fetch() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    knit(&workspace, ["bundle", "remote made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/remote-made/backend");
    append_line(&feature.join("app.txt"), "work from another machine");
    knit(&workspace, ["commit", "--all", "-m", "Remote-machine work"]);
    knit(&workspace, ["push", "--set-upstream"]);
    let artifact_path = workspace.join(".knit/bundles/remote-made.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    fs::remove_file(&artifact_path).unwrap();
    fs::remove_dir_all(workspace.join(".knit/worktrees/remote-made")).unwrap();
    git(&backend, ["worktree", "prune"]);
    git(&backend, ["branch", "-D", "knit/remote-made"]);

    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-1",
                "slug": "remote-made",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "hash-1", "payload": payload},
            }],
            "historyEvents": [],
        }
    });
    let base_url = spawn_fake_remote_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let fetch_output = knit_with_env(&workspace, ["fetch", "--mode", "knit"], &env);
    assert!(fetch_output.contains("new"), "{fetch_output}");
    knit(&workspace, ["switch", "remote-made", "--workspace"]);

    let pull = knit_with_env(&workspace, ["pull"], &env);
    assert!(pull.contains("materialized 1 checkout(s)"), "{pull}");
    let text = fs::read_to_string(feature.join("app.txt")).unwrap();
    assert!(text.contains("work from another machine"), "{text}");

    // A second pull has nothing left to do.
    let again = knit_with_env(&workspace, ["pull"], &env);
    assert!(again.contains("up to date"), "{again}");

    fs::remove_dir_all(root).unwrap();
}

/// `knit fetch` fast-forwards the bundle artifact without touching checkouts.
/// The following `knit pull` must still fast-forward the feature checkout onto
/// origin instead of treating the already-current artifact as "nothing to do".
#[test]
fn pull_fast_forwards_checkouts_after_fetch_advanced_the_artifact() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    knit(&workspace, ["bundle", "remote made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/remote-made/backend");
    append_line(&feature.join("app.txt"), "first line");
    knit(&workspace, ["commit", "--all", "-m", "First"]);
    knit(&workspace, ["push", "--set-upstream"]);
    let artifact_path = workspace.join(".knit/bundles/remote-made.bundle.json");
    let artifact_v1 = fs::read_to_string(&artifact_path).unwrap();

    // The second commit plays the collaborator: origin and the remote artifact
    // advance past the state this workspace is then rewound to.
    append_line(&feature.join("app.txt"), "second line");
    knit(&workspace, ["commit", "--all", "-m", "Second"]);
    knit(&workspace, ["push"]);
    let payload_v2: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    fs::write(&artifact_path, artifact_v1).unwrap();
    git(&feature, ["reset", "--hard", "HEAD~1"]);

    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-1",
                "slug": "remote-made",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "hash-2", "payload": payload_v2},
            }],
            "historyEvents": [],
        }
    });
    let base_url = spawn_fake_remote_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let fetch_output = knit_with_env(&workspace, ["fetch", "--mode", "knit"], &env);
    assert!(fetch_output.contains("updated"), "{fetch_output}");
    let stale = fs::read_to_string(feature.join("app.txt")).unwrap();
    assert!(!stale.contains("second line"), "{stale}");

    let pull = knit_with_env(&workspace, ["pull"], &env);
    assert!(pull.contains("fast-forwarded 1 checkout(s)"), "{pull}");
    let text = fs::read_to_string(feature.join("app.txt")).unwrap();
    assert!(text.contains("second line"), "{text}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn archive_and_restore_sync_lifecycle_state_to_remote() {
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
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_push_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    knit(&workspace, ["bundle", "quick fix", "--repo", "backend"]);
    knit_with_env(&workspace, ["bundle", "archive", "quick-fix"], &env);

    let record = fake_dir.join("artifact-quick-fix.states");
    let states = fs::read_to_string(&record).expect("archive should push the artifact");
    assert_eq!(states.lines().last(), Some("archived"), "{states}");

    knit_with_env(&workspace, ["bundle", "restore", "quick-fix"], &env);
    let states = fs::read_to_string(&record).unwrap();
    assert_eq!(states.lines().last(), Some("open"), "{states}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_bundles_sweeps_open_and_archived_artifacts() {
    let root = unique_temp_dir();
    let (remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_push_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    knit(&workspace, ["bundle", "alpha work", "--repo", "backend"]);
    knit(&workspace, ["bundle", "beta work", "--repo", "backend"]);
    // No token in the environment here, so the archive's own remote sync
    // warn-skips and the later sweep is what carries the state.
    knit(&workspace, ["bundle", "archive", "beta-work"]);

    let output = knit_with_env(&workspace, ["sync", "push", "--bundles"], &env);
    assert!(output.contains("bundle artifact(s)"), "{output}");

    let alpha = fs::read_to_string(fake_dir.join("artifact-alpha-work.states")).unwrap();
    assert_eq!(alpha.lines().last(), Some("open"), "{alpha}");
    let beta = fs::read_to_string(fake_dir.join("artifact-beta-work.states")).unwrap();
    assert_eq!(beta.lines().last(), Some("archived"), "{beta}");

    // A successful upsert records the server-owned bundle id locally and in
    // the uploaded artifact, so UI clients never substitute the local slug.
    let alpha_bundle: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/alpha-work.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(alpha_bundle["syncTargets"][0]["remote"], "hosted");
    assert_eq!(alpha_bundle["syncTargets"][0]["bundleId"], "rb-alpha-work");
    assert_eq!(alpha_bundle["syncTargets"][0]["apiUrl"], base_url);
    let pushed_body: serde_json::Value = serde_json::from_str(
        fs::read_to_string(fake_dir.join("artifact-alpha-work.bodies"))
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        pushed_body["payload"]["syncTargets"][0]["bundleId"],
        "rb-alpha-work"
    );

    // The open bundle's feature branch went to git origin with the artifact;
    // the archived bundle stayed artifact-only.
    assert!(
        git_success(
            &remote,
            ["rev-parse", "--verify", "refs/heads/knit/alpha-work"]
        ),
        "open bundle branch should be on origin after the sweep"
    );
    assert!(
        !git_success(
            &remote,
            ["rev-parse", "--verify", "refs/heads/knit/beta-work"]
        ),
        "archived bundle sweep must not push branches"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_pushes_open_bundle_branches_before_artifact() {
    let root = unique_temp_dir();
    let (remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_push_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    knit(&workspace, ["bundle", "alpha work", "--repo", "backend"]);
    let checkout = workspace.join(".knit/worktrees/alpha-work/backend");
    append_line(&checkout.join("app.txt"), "alpha change");
    knit(&workspace, ["commit", "--all", "-m", "Alpha change"]);
    let feature_head = git(&checkout, ["rev-parse", "HEAD"]);
    assert!(
        !git_success(
            &remote,
            ["rev-parse", "--verify", "refs/heads/knit/alpha-work"]
        ),
        "the feature branch must not be on origin before the sync push"
    );

    let output = knit_with_env(&workspace, ["sync", "push", "--bundles"], &env);
    assert!(output.contains("origin/knit/alpha-work"), "{output}");

    // Branch first: origin now holds the recorded head.
    assert_eq!(
        git(&remote, ["rev-parse", "refs/heads/knit/alpha-work"]),
        feature_head
    );
    // ...and the artifact still made it to the sync remote.
    let states = fs::read_to_string(fake_dir.join("artifact-alpha-work.states")).unwrap();
    assert_eq!(states.lines().last(), Some("open"), "{states}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_skips_open_bundle_artifact_when_branch_push_fails() {
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
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_push_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    // The bad bundle is created first so the workspace fallback resolves the
    // healthy bundle as active and the broken one is reached by the sweep.
    knit(&workspace, ["bundle", "bad work", "--repo", "frontend"]);
    knit(&workspace, ["bundle", "good work", "--repo", "backend"]);
    // Break the bad bundle's git origin after creation, so its branch can
    // neither be verified nor pushed. The generated worktree shares the
    // source checkout's remote config.
    git(
        &frontend,
        [
            "remote",
            "set-url",
            "origin",
            root.join("missing.git").to_str().unwrap(),
        ],
    );

    let output = knit_with_env(&workspace, ["sync", "push", "--bundles"], &env);
    assert!(output.contains("sync skipped (bad-work)"), "{output}");

    // The unreachable bundle's artifact never reached the sync remote...
    assert!(
        !fake_dir.join("artifact-bad-work.states").exists(),
        "bad-work artifact must not be uploaded when its branches cannot be pushed"
    );
    // ...while the healthy bundle in the same sweep pushed branch + artifact.
    let states = fs::read_to_string(fake_dir.join("artifact-good-work.states")).unwrap();
    assert_eq!(states.lines().last(), Some("open"), "{states}");
    assert!(
        git_success(
            &backend_remote,
            ["rev-parse", "--verify", "refs/heads/knit/good-work"]
        ),
        "healthy bundle branch should be on origin after the sweep"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_walks_sync_remotes_past_unreachable_one() {
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
    knit(&workspace, ["bundle", "feature one", "--repo", "backend"]);

    // With no sync-remotes config, every configured remote is a sync remote.
    // `dead` sorts first and refuses connections; `live` serves a valid export.
    let dead_url = unreachable_remote_url();
    let live_url = spawn_fake_remote_with_body(
        "{\"data\":{\"project\":{\"slug\":\"demo\"},\"knitProject\":null,\"repositories\":[],\"bundles\":[],\"historyEvents\":[]}}".to_string(),
    );
    knit(&workspace, ["remote", "add", "dead", &dead_url]);
    knit(&workspace, ["remote", "add", "live", &live_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let report = knit_with_env(&workspace, ["pull", "--main", "--bundles"], &env);
    assert!(report.contains("remote dead unavailable"), "{report}");
    assert!(report.contains("Current checkouts:"), "{report}");
    assert!(report.contains("feature-one"), "{report}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_continues_when_every_sync_remote_is_unreachable() {
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
    knit(&workspace, ["bundle", "feature one", "--repo", "backend"]);
    knit(
        &workspace,
        ["remote", "add", "dead", &unreachable_remote_url()],
    );
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    // The offline remote is reported, and the git side still runs.
    let report = knit_with_env(&workspace, ["pull", "--main", "--bundles"], &env);
    assert!(report.contains("No sync remote reachable"), "{report}");
    assert!(report.contains("Current checkouts:"), "{report}");
    assert!(report.contains("no sync remote available"), "{report}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_with_explicit_remote_still_fails_hard() {
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
    knit(&workspace, ["bundle", "feature one", "--repo", "backend"]);
    knit(
        &workspace,
        ["remote", "add", "dead", &unreachable_remote_url()],
    );
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let failure = knit_fails_with_env(
        &workspace,
        ["pull", "--main", "--bundles", "--remote", "dead"],
        &env,
    );
    assert!(failure.contains("Remote request failed"), "{failure}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_fans_out_to_every_configured_remote_by_default() {
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
    let fake_a = root.join("fake-a");
    let fake_b = root.join("fake-b");
    let url_a = spawn_fake_remote_push_api(&fake_a);
    let url_b = spawn_fake_remote_push_api(&fake_b);
    knit(&workspace, ["remote", "add", "alpha", &url_a]);
    knit(&workspace, ["remote", "add", "beta", &url_b]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    knit(&workspace, ["bundle", "quick fix", "--repo", "backend"]);
    let output = knit_with_env(&workspace, ["sync", "push", "--bundles"], &env);
    assert!(output.contains("alpha"), "{output}");
    assert!(output.contains("beta"), "{output}");

    let alpha = fs::read_to_string(fake_a.join("artifact-quick-fix.states")).unwrap();
    assert_eq!(alpha.lines().last(), Some("open"), "{alpha}");
    let beta = fs::read_to_string(fake_b.join("artifact-quick-fix.states")).unwrap();
    assert_eq!(beta.lines().last(), Some("open"), "{beta}");

    fs::remove_dir_all(root).unwrap();
}

/// The last artifact body the fake sync remote received for a bundle slug.
fn last_artifact_body(dir: &std::path::Path, slug: &str) -> serde_json::Value {
    let raw = fs::read_to_string(dir.join(format!("artifact-{slug}.bodies"))).unwrap();
    serde_json::from_str(raw.lines().last().unwrap()).unwrap()
}

/// Scaffold a workspace with one project repo and a fake sync remote, ready
/// for artifact force-push tests. Returns (root, workspace, fake_dir).
fn force_push_scaffold(
    with_bundles: &[&str],
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
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
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_push_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    for title in with_bundles {
        knit(&workspace, ["bundle", title, "--repo", "backend"]);
    }
    (root, workspace, fake_dir)
}

#[test]
fn project_push_prune_deletes_remote_repos_absent_from_local_shape() {
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
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_push_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);

    // Remote lists backend (kept) plus two orphan records absent locally: one
    // carrying localId in metadata, one only a name.
    fs::write(
        fake_dir.join("repositories.json"),
        "{\"data\":[\
           {\"id\":\"uuid-backend\",\"localId\":\"backend\",\"name\":\"backend\",\"metadata\":{}},\
           {\"id\":\"uuid-oldrepo\",\"localId\":null,\"name\":\"oldrepo\",\"metadata\":{\"localId\":\"oldrepo\"}},\
           {\"id\":\"uuid-legacy\",\"localId\":null,\"name\":\"legacy\",\"metadata\":{}}]}",
    )
    .unwrap();

    let env = [("KNIT_REMOTE_TOKEN", "test-token")];
    let output = knit_with_env(&workspace, ["project", "push", "--prune"], &env);
    assert!(output.contains("pruned"), "{output}");
    assert!(output.contains("oldrepo"), "{output}");
    assert!(output.contains("legacy"), "{output}");

    let deleted = fs::read_to_string(fake_dir.join("deleted-repositories.txt")).unwrap();
    let mut deleted_ids: Vec<&str> = deleted.lines().collect();
    deleted_ids.sort();
    assert_eq!(deleted_ids, vec!["uuid-legacy", "uuid-oldrepo"]);
    // backend is in the local shape, so it must never be deleted.
    assert!(!deleted.contains("uuid-backend"), "{deleted}");

    // Without --prune, no deletes happen.
    let _ = fs::remove_file(fake_dir.join("deleted-repositories.txt"));
    knit_with_env(&workspace, ["project", "push"], &env);
    assert!(!fake_dir.join("deleted-repositories.txt").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_without_force_hits_409_and_hints_at_force_with_lease() {
    let (root, workspace, fake_dir) = force_push_scaffold(&["quick fix"]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];
    fs::write(fake_dir.join("enforce-fast-forward"), "").unwrap();

    let failure = knit_fails_with_env(&workspace, ["sync", "push", "--bundles"], &env);
    assert!(
        failure.contains("knit sync push --bundles --force-with-lease"),
        "{failure}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_force_overwrites_remote_ledger_unconditionally() {
    let (root, workspace, fake_dir) = force_push_scaffold(&["quick fix"]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];
    // A remote whose ledger is ahead refuses every non-forced push.
    fs::write(fake_dir.join("enforce-fast-forward"), "").unwrap();

    let output = knit_with_env(&workspace, ["sync", "push", "--bundles", "--force"], &env);
    assert!(output.contains("pushed (forced)"), "{output}");

    let body = last_artifact_body(&fake_dir, "quick-fix");
    assert_eq!(body["force"], serde_json::json!(true), "{body}");
    assert!(body.get("expectedArtifactHash").is_none(), "{body}");
    assert_eq!(body["kind"], serde_json::json!("bundle"), "{body}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_force_with_lease_fetches_hash_and_cas_accepts() {
    let (root, workspace, fake_dir) = force_push_scaffold(&["quick fix"]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];
    fs::write(fake_dir.join("enforce-fast-forward"), "").unwrap();
    fs::write(fake_dir.join("current-artifact-hash"), "lease-hash-1").unwrap();

    let output = knit_with_env(
        &workspace,
        ["sync", "push", "--bundles", "--force-with-lease"],
        &env,
    );
    assert!(output.contains("pushed (forced)"), "{output}");

    let body = last_artifact_body(&fake_dir, "quick-fix");
    assert_eq!(body["force"], serde_json::json!(true), "{body}");
    assert_eq!(
        body["expectedArtifactHash"],
        serde_json::json!("lease-hash-1"),
        "{body}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_force_with_lease_mismatch_fails_each_bundle() {
    // Two open bundles from the workspace root: no active bundle resolves and
    // the project-wide sweep carries both, so the per-bundle lease failures
    // are collected instead of aborting the run at the first one.
    let (root, workspace, fake_dir) = force_push_scaffold(&["alpha work", "beta work"]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];
    // The GET for the lease sees hash-a; by the time the POST lands the
    // remote is on hash-b — a concurrent push in the window.
    fs::write(fake_dir.join("current-artifact-hash"), "hash-a").unwrap();
    fs::write(fake_dir.join("post-current-artifact-hash"), "hash-b").unwrap();

    let failure = knit_fails_with_env(
        &workspace,
        ["sync", "push", "--bundles", "--force-with-lease"],
        &env,
    );
    assert!(
        failure.contains("alpha-work: remote artifact changed since fetch"),
        "{failure}"
    );
    assert!(
        failure.contains("beta-work: remote artifact changed since fetch"),
        "{failure}"
    );
    assert!(failure.contains("hash-b"), "{failure}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_force_flags_conflict() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let failure = knit_fails(
        &workspace,
        ["sync", "push", "--bundles", "--force", "--force-with-lease"],
    );
    assert!(failure.contains("cannot be used with"), "{failure}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_push_force_requires_a_bundle_target() {
    let (root, workspace, _fake_dir) = force_push_scaffold(&["quick fix"]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let failure = knit_fails_with_env(&workspace, ["sync", "push", "--views", "--force"], &env);
    assert!(
        failure.contains("apply only to bundle artifacts"),
        "{failure}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn push_force_with_lease_propagates_into_the_artifact_sync() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _collab) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_push_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    knit(&workspace, ["bundle", "quick fix", "--repo", "backend"]);

    // A plain push sends a plain artifact body: no force fields.
    knit_with_env(&workspace, ["push", "--set-upstream"], &env);
    let body = last_artifact_body(&fake_dir, "quick-fix");
    assert!(body.get("force").is_none(), "{body}");
    assert!(body.get("expectedArtifactHash").is_none(), "{body}");

    // A forced branch push carries the same force mode into the artifact
    // sync: lease hash fetched from the sync remote, then compare-and-swap.
    fs::write(fake_dir.join("current-artifact-hash"), "lease-hash-9").unwrap();
    let output = knit_with_env(&workspace, ["push", "--force-with-lease"], &env);
    assert!(output.contains("pushed (forced)"), "{output}");
    let body = last_artifact_body(&fake_dir, "quick-fix");
    assert_eq!(body["force"], serde_json::json!(true), "{body}");
    assert_eq!(
        body["expectedArtifactHash"],
        serde_json::json!("lease-hash-9"),
        "{body}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// The slim project export carries artifact metadata only, so a project-wide
/// bundle pull downloads each payload it needs on its own — and never touches
/// the payloads of records it skips.
#[test]
fn sync_pull_fetches_each_bundle_artifact_from_the_slim_export() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    // Author the bundle another machine would have pushed, then erase it here.
    knit(&workspace, ["bundle", "remote made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/remote-made/backend");
    append_line(&feature.join("app.txt"), "work from another machine");
    knit(&workspace, ["commit", "--all", "-m", "Remote-machine work"]);
    let artifact_path = workspace.join(".knit/bundles/remote-made.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    fs::remove_file(&artifact_path).unwrap();
    fs::remove_dir_all(workspace.join(".knit/worktrees/remote-made")).unwrap();

    let mut archived_payload = payload.clone();
    archived_payload["id"] = serde_json::json!("old-landed");

    // The export identifies each bundle's current artifact but carries no
    // payload — exactly what a server serving `artifacts=none` returns.
    let fake_dir = root.join("fake-remote");
    fs::create_dir_all(&fake_dir).unwrap();
    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [
                {
                    "id": "rb-1",
                    "slug": "remote-made",
                    "lifecycleState": "open",
                    "currentArtifact": {"artifactHash": "hash-remote", "sizeBytes": 42},
                },
                {
                    "id": "rb-3",
                    "slug": "old-landed",
                    "lifecycleState": "archived",
                    "currentArtifact": {"artifactHash": "hash-old", "sizeBytes": 42},
                },
            ],
            "historyEvents": [],
        }
    });
    fs::write(fake_dir.join("export.json"), export.to_string()).unwrap();
    fs::write(
        fake_dir.join("bundle-rb-1.json"),
        serde_json::json!({
            "data": {
                "id": "rb-1",
                "slug": "remote-made",
                "currentArtifact": {"artifactHash": "hash-remote", "payload": payload},
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        fake_dir.join("bundle-rb-3.json"),
        serde_json::json!({
            "data": {
                "id": "rb-3",
                "slug": "old-landed",
                "currentArtifact": {"artifactHash": "hash-old", "payload": archived_payload},
            }
        })
        .to_string(),
    )
    .unwrap();

    let base_url = spawn_fake_remote_bundle_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(
        &workspace,
        ["sync", "pull", "--bundles", "--remote", "hosted"],
        &env,
    );
    assert!(output.contains("fetched"), "{output}");
    assert!(artifact_path.exists(), "{output}");

    // Only the bundle that had to be localized was downloaded; the archived
    // record with no local artifact is skipped before any payload is asked for.
    assert_eq!(
        recorded_artifact_fetches(&fake_dir),
        vec!["rb-1".to_string()]
    );

    // The pulled artifact records which remote artifact it is in sync with.
    let localized: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    assert_eq!(
        localized["syncTargets"][0]["artifactHash"],
        serde_json::json!("hash-remote")
    );

    fs::remove_dir_all(root).unwrap();
}

/// Once a bundle records the remote artifact hash it is in sync with, later
/// pulls decide "nothing new" from the slim export alone: the payload is never
/// downloaded again.
#[test]
fn pull_skips_the_artifact_fetch_when_the_recorded_hash_matches() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(&workspace, ["bundle", "remote made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/remote-made/backend");
    append_line(&feature.join("app.txt"), "shared work");
    knit(&workspace, ["commit", "--all", "-m", "Shared work"]);
    knit(&workspace, ["push", "--set-upstream"]);

    let artifact_path = workspace.join(".knit/bundles/remote-made.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();

    let fake_dir = root.join("fake-remote");
    fs::create_dir_all(&fake_dir).unwrap();
    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-1",
                "slug": "remote-made",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "hash-same", "sizeBytes": 42},
            }],
            "historyEvents": [],
        }
    });
    fs::write(fake_dir.join("export.json"), export.to_string()).unwrap();
    fs::write(
        fake_dir.join("bundle-rb-1.json"),
        serde_json::json!({
            "data": {
                "id": "rb-1",
                "slug": "remote-made",
                "currentArtifact": {"artifactHash": "hash-same", "payload": payload},
            }
        })
        .to_string(),
    )
    .unwrap();

    let base_url = spawn_fake_remote_bundle_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    // First pull: nothing is recorded yet, so the payload is downloaded once
    // and the hash it came with is written onto the local artifact.
    knit_with_env(&workspace, ["pull"], &env);
    assert_eq!(
        recorded_artifact_fetches(&fake_dir),
        vec!["rb-1".to_string()]
    );
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    assert_eq!(
        saved["syncTargets"][0]["artifactHash"],
        serde_json::json!("hash-same")
    );

    // Second pull: the export's hash is the recorded one, so no payload is
    // fetched at all.
    let again = knit_with_env(&workspace, ["pull"], &env);
    assert!(again.contains("up to date"), "{again}");
    assert_eq!(
        recorded_artifact_fetches(&fake_dir),
        vec!["rb-1".to_string()],
        "a matching hash must not download the payload again"
    );

    fs::remove_dir_all(root).unwrap();
}

/// A server that ignores the slim request (an older deployment) still inlines
/// every payload in the export. The client must use those and never issue a
/// per-bundle fetch — here the fake serves no bundle route at all, so a fetch
/// would fail the pull.
#[test]
fn sync_pull_uses_the_payload_an_older_server_inlined() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(&workspace, ["bundle", "remote made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/remote-made/backend");
    append_line(&feature.join("app.txt"), "work from another machine");
    knit(&workspace, ["commit", "--all", "-m", "Remote-machine work"]);
    let artifact_path = workspace.join(".knit/bundles/remote-made.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    fs::remove_file(&artifact_path).unwrap();
    fs::remove_dir_all(workspace.join(".knit/worktrees/remote-made")).unwrap();

    let fake_dir = root.join("fake-remote");
    fs::create_dir_all(&fake_dir).unwrap();
    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-1",
                "slug": "remote-made",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "hash-inline", "payload": payload},
            }],
            "historyEvents": [],
        }
    });
    fs::write(fake_dir.join("export.json"), export.to_string()).unwrap();

    let base_url = spawn_fake_remote_bundle_api(&fake_dir);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(
        &workspace,
        ["sync", "pull", "--bundles", "--remote", "hosted"],
        &env,
    );
    assert!(output.contains("fetched"), "{output}");
    assert!(artifact_path.exists(), "{output}");
    assert!(
        recorded_artifact_fetches(&fake_dir).is_empty(),
        "an inlined payload must not trigger a per-bundle fetch"
    );

    fs::remove_dir_all(root).unwrap();
}

// ---------------------------------------------------------------------------
// Pull-side project reconcile: membership vs observed inventory
// ---------------------------------------------------------------------------

/// Export body whose `knitProject.repos` (deliberate membership) and
/// `repositories` (observed inventory) can diverge — the situation orphaned
/// bundle-projected records used to create.
fn membership_export(
    membership_repos: serde_json::Value,
    repositories: serde_json::Value,
    omitted: u64,
) -> String {
    serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": {
                "schemaVersion": "0.1",
                "kind": "KnitProject",
                "id": "demo",
                "createdAt": "2026-01-01T00:00:00.000Z",
                "updatedAt": "2026-01-01T00:00:00.000Z",
                "repos": membership_repos,
            },
            "repositories": repositories,
            "omittedRepositoryCount": omitted,
            "bundles": [],
            "historyEvents": [],
        }
    })
    .to_string()
}

fn project_repo_ids(workspace: &Path) -> Vec<String> {
    let project: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/projects/demo.project.json")).unwrap(),
    )
    .unwrap();
    project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"].as_str().unwrap().to_string())
        .collect()
}

fn reconcile_scaffold(root: &Path, repos: &[&str]) -> PathBuf {
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "demo"]);
    for label in repos {
        let repo = root.join(label);
        init_repo(&repo, label);
        knit(
            &workspace,
            ["project", "add", label, repo.to_str().unwrap()],
        );
    }
    knit(&workspace, ["bundle", "seed work", "--repo", repos[0]]);
    workspace
}

#[test]
fn pull_reconcile_ignores_inventory_records_outside_membership() {
    let root = unique_temp_dir();
    let workspace = reconcile_scaffold(&root, &["backend"]);

    // The incident shape: a ghost record in the inventory (projected by a
    // since-deleted bundle, its GitHub repo never created) that membership
    // does not claim. It must be neither cloned nor added — nor even probed.
    let export = membership_export(
        serde_json::json!([
            {"id": "backend", "path": "", "remote": root.join("backend").to_str().unwrap(), "baseBranch": "main"},
        ]),
        serde_json::json!([
            {"localId": "backend", "name": "backend", "remoteUrl": root.join("backend").to_str().unwrap(), "metadata": {}},
            {"localId": "ghost", "name": "ghost", "remoteUrl": "https://github.com/nobody/ghost.git", "visibility": "private", "metadata": {"source": "bundle_projection"}},
        ]),
        0,
    );
    let base_url = spawn_fake_remote_with_body(export);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(&workspace, ["pull", "--bundles"], &env);
    assert!(!output.contains("ghost"), "{output}");
    assert!(!output.contains("Project repo add failed"), "{output}");
    assert!(!workspace.join("ghost").exists());
    assert_eq!(project_repo_ids(&workspace), vec!["backend"]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_reconcile_keeps_removals_when_an_add_fails() {
    let root = unique_temp_dir();
    let workspace = reconcile_scaffold(&root, &["backend", "oldrepo"]);

    // Membership says: drop oldrepo, add newrepo — but newrepo's remote does
    // not exist. The failed add must keep the removal from being persisted,
    // and a public repo's failure must not be blamed on credentials.
    let missing_remote = root.join("does-not-exist");
    let export = membership_export(
        serde_json::json!([
            {"id": "backend", "path": "", "remote": root.join("backend").to_str().unwrap(), "baseBranch": "main"},
            {"id": "newrepo", "path": "", "remote": missing_remote.to_str().unwrap(), "baseBranch": "main"},
        ]),
        serde_json::json!([
            {"localId": "backend", "name": "backend", "remoteUrl": root.join("backend").to_str().unwrap(), "metadata": {}},
            {"localId": "newrepo", "name": "newrepo", "remoteUrl": missing_remote.to_str().unwrap(), "visibility": "public", "metadata": {}},
        ]),
        0,
    );
    let base_url = spawn_fake_remote_with_body(export);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(&workspace, ["pull", "--bundles"], &env);
    assert!(output.contains("Project repo add failed:"), "{output}");
    assert!(output.contains("repository not found on the forge"), "{output}");
    assert!(!output.contains("no HTTPS git access"), "{output}");
    assert!(
        output.contains("keeping 1 removed repo(s) locally: oldrepo"),
        "{output}"
    );

    let mut ids = project_repo_ids(&workspace);
    ids.sort();
    assert_eq!(ids, vec!["backend", "oldrepo"]);
    assert!(!workspace.join("newrepo").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_reconcile_applies_adds_and_removals_together() {
    let root = unique_temp_dir();
    let workspace = reconcile_scaffold(&root, &["backend", "oldrepo"]);

    let newrepo = root.join("newrepo");
    init_repo(&newrepo, "newrepo");

    let export = membership_export(
        serde_json::json!([
            {"id": "backend", "path": "", "remote": root.join("backend").to_str().unwrap(), "baseBranch": "main"},
            {"id": "newrepo", "path": "", "remote": newrepo.to_str().unwrap(), "baseBranch": "main"},
        ]),
        serde_json::json!([
            {"localId": "backend", "name": "backend", "remoteUrl": root.join("backend").to_str().unwrap(), "metadata": {}},
            {"localId": "newrepo", "name": "newrepo", "remoteUrl": newrepo.to_str().unwrap(), "defaultBranch": "main", "visibility": "public", "metadata": {}},
        ]),
        0,
    );
    let base_url = spawn_fake_remote_with_body(export);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(&workspace, ["pull", "--bundles"], &env);
    assert!(output.contains("syncing membership from remote (+1 / -1)"), "{output}");
    assert!(output.contains("added"), "{output}");
    assert!(output.contains("newrepo"), "{output}");
    assert!(output.contains("removed"), "{output}");
    assert!(output.contains("oldrepo"), "{output}");

    let mut ids = project_repo_ids(&workspace);
    ids.sort();
    assert_eq!(ids, vec!["backend", "newrepo"]);
    assert!(workspace.join("newrepo").join("app.txt").exists());
    // The removed repo's checkout on disk is left alone.
    assert!(root.join("oldrepo").join("app.txt").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_reconcile_skips_removals_on_incomplete_export() {
    let root = unique_temp_dir();
    let workspace = reconcile_scaffold(&root, &["backend", "oldrepo"]);

    // The server admits it withheld a repo from this viewer: an absent repo is
    // indistinguishable from a hidden one, so nothing may be dropped.
    let export = membership_export(
        serde_json::json!([
            {"id": "backend", "path": "", "remote": root.join("backend").to_str().unwrap(), "baseBranch": "main"},
        ]),
        serde_json::json!([
            {"localId": "backend", "name": "backend", "remoteUrl": root.join("backend").to_str().unwrap(), "metadata": {}},
        ]),
        1,
    );
    let base_url = spawn_fake_remote_with_body(export);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(&workspace, ["pull", "--bundles"], &env);
    assert!(!output.contains("Project repo:"), "{output}");

    let mut ids = project_repo_ids(&workspace);
    ids.sort();
    assert_eq!(ids, vec!["backend", "oldrepo"]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_reconcile_skipped_without_membership_payload() {
    let root = unique_temp_dir();
    let workspace = reconcile_scaffold(&root, &["backend"]);

    // Degenerate/old-server export without a knitProject payload: inventory
    // records alone must not mutate the local project.
    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [
                {"localId": "ghost", "name": "ghost", "remoteUrl": "https://github.com/nobody/ghost.git", "metadata": {}},
            ],
            "bundles": [],
            "historyEvents": [],
        }
    })
    .to_string();
    let base_url = spawn_fake_remote_with_body(export);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(&workspace, ["pull", "--bundles"], &env);
    assert!(!output.contains("ghost"), "{output}");
    assert_eq!(project_repo_ids(&workspace), vec!["backend"]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_reconcile_reports_forge_missing_repos_honestly() {
    let root = unique_temp_dir();
    let workspace = reconcile_scaffold(&root, &["backend"]);

    // The sync remote's owner-credentialed visibility refresh marked the repo
    // missing on its forge: fail fast, with no credentials hint.
    let export = membership_export(
        serde_json::json!([
            {"id": "backend", "path": "", "remote": root.join("backend").to_str().unwrap(), "baseBranch": "main"},
            {"id": "doomed", "path": "", "remote": "https://github.com/nobody/doomed.git", "baseBranch": "main"},
        ]),
        serde_json::json!([
            {"localId": "backend", "name": "backend", "remoteUrl": root.join("backend").to_str().unwrap(), "metadata": {}},
            {"localId": "doomed", "name": "doomed", "remoteUrl": "https://github.com/nobody/doomed.git", "visibility": "private", "metadata": {"forgeState": "missing"}},
        ]),
        0,
    );
    let base_url = spawn_fake_remote_with_body(export);
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(&workspace, ["pull", "--bundles"], &env);
    assert!(
        output.contains("repository does not exist on its forge"),
        "{output}"
    );
    assert!(!output.contains("no HTTPS git access"), "{output}");
    assert_eq!(project_repo_ids(&workspace), vec!["backend"]);

    fs::remove_dir_all(root).unwrap();
}
