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
fn push_skips_missing_implicit_knithub_remote_after_git_branch_push() {
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
    config["syncRemote"] = serde_json::json!("svartal");
    config["syncRemotes"] = serde_json::json!(["svartal"]);
    fs::write(
        &config_path,
        format!("{}\n", serde_json::to_string_pretty(&config).unwrap()),
    )
    .unwrap();

    let push = knit(&workspace, ["push", "backend"]);
    assert!(push.contains("backend"), "{push}");
    assert!(push.contains("KnitHub sync skipped (svartal):"), "{push}");
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
    let base_url = spawn_fake_knithub_with_body(export.to_string());
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
    // another machine would have pushed to KnitHub — then erase it locally as
    // if it had never existed here.
    knit(&workspace, ["bundle", "svartal made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/svartal-made/backend");
    append_line(&feature.join("app.txt"), "work from another machine");
    knit(&workspace, ["commit", "--all", "-m", "Remote-machine work"]);
    let artifact_path = workspace.join(".knit/bundles/svartal-made.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    fs::remove_file(&artifact_path).unwrap();
    fs::remove_dir_all(workspace.join(".knit/worktrees/svartal-made")).unwrap();

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
                    "slug": "svartal-made",
                    "lifecycleState": "open",
                    "currentArtifact": {"artifactHash": "hash-svartal", "payload": payload},
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
    let base_url = spawn_fake_knithub_with_body(export.to_string());
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
    assert!(list.contains("svartal-made"), "{list}");
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
    let base_url = spawn_fake_knithub_with_body(export.to_string());
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
/// KnitHub side lists each remote bundle with its repo -> branch mapping.
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
    knit(&workspace, ["bundle", "svartal made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/svartal-made/backend");
    append_line(&feature.join("app.txt"), "work from another machine");
    knit(&workspace, ["commit", "--all", "-m", "Remote-machine work"]);
    knit(&workspace, ["push", "--set-upstream"]);
    let artifact_path = workspace.join(".knit/bundles/svartal-made.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    fs::remove_file(&artifact_path).unwrap();
    fs::remove_dir_all(workspace.join(".knit/worktrees/svartal-made")).unwrap();
    git(&backend, ["worktree", "prune"]);
    git(&backend, ["branch", "-D", "knit/svartal-made"]);

    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-1",
                "slug": "svartal-made",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "hash-1", "payload": payload},
            }],
            "historyEvents": [],
        }
    });
    let base_url = spawn_fake_knithub_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let output = knit_with_env(&workspace, ["fetch"], &env);
    assert!(output.contains("origin/main"), "{output}");
    assert!(output.contains("backend -> knit/svartal-made"), "{output}");
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

    knit(&workspace, ["bundle", "svartal made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/svartal-made/backend");
    append_line(&feature.join("app.txt"), "work from another machine");
    knit(&workspace, ["commit", "--all", "-m", "Remote-machine work"]);
    knit(&workspace, ["push", "--set-upstream"]);
    let artifact_path = workspace.join(".knit/bundles/svartal-made.bundle.json");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifact_path).unwrap()).unwrap();
    fs::remove_file(&artifact_path).unwrap();
    fs::remove_dir_all(workspace.join(".knit/worktrees/svartal-made")).unwrap();
    git(&backend, ["worktree", "prune"]);
    git(&backend, ["branch", "-D", "knit/svartal-made"]);

    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-1",
                "slug": "svartal-made",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "hash-1", "payload": payload},
            }],
            "historyEvents": [],
        }
    });
    let base_url = spawn_fake_knithub_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    let fetch_output = knit_with_env(&workspace, ["fetch", "--mode", "knit"], &env);
    assert!(fetch_output.contains("new"), "{fetch_output}");
    knit(&workspace, ["switch", "svartal-made", "--workspace"]);

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

    knit(&workspace, ["bundle", "svartal made", "--repo", "backend"]);
    let feature = workspace.join(".knit/worktrees/svartal-made/backend");
    append_line(&feature.join("app.txt"), "first line");
    knit(&workspace, ["commit", "--all", "-m", "First"]);
    knit(&workspace, ["push", "--set-upstream"]);
    let artifact_path = workspace.join(".knit/bundles/svartal-made.bundle.json");
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
                "slug": "svartal-made",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "hash-2", "payload": payload_v2},
            }],
            "historyEvents": [],
        }
    });
    let base_url = spawn_fake_knithub_with_body(export.to_string());
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
