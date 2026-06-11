mod common;

use common::*;
use std::fs;

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
fn pull_main_fast_forwards_project_repos_and_reports() {
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

    let report = knit(&workspace, ["pull", "--main"]);
    assert!(report.contains("Main repos:"));
    assert!(report.contains("backend"));
    assert!(report.contains(&backend_sha[..7]));
    assert!(report.contains("frontend"));
    assert!(report.contains("skipped"));

    // Backend's source checkout fast-forwarded; the dirty repo was left alone.
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), backend_sha);

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
    assert!(report.contains("Main repos:"));
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
    assert!(report.contains("no KnitHub remote configured"));
    // --bundles alone does not touch project main repos.
    assert!(!report.contains("Main repos:"));

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
    append_line(&frontend_feature.join("app.txt"), "parallel frontend commit");

    let gate = root.join("commit-gate");
    install_parallel_gate_hook(&backend_feature, "pre-commit", &gate, "backend", "frontend");
    install_parallel_gate_hook(&frontend_feature, "pre-commit", &gate, "frontend", "backend");

    let commit = knit(
        &workspace,
        ["commit", "--all", "-m", "Parallel commit"],
    );
    assert!(commit.contains("backend"));
    assert!(commit.contains("frontend"));
    assert!(commit.contains("Recorded commit group"));

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
    knit(&workspace, ["bundle", "venue capacity", "--repo", "backend"]);

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
    let base_url = spawn_fake_knithub_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "knithub", &base_url]);
    let env = [("KNITHUB_TOKEN", "test-token")];

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
