mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn latest_node_of_type<'a>(bundle: &'a Value, node_type: &str) -> &'a Value {
    bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|node| node["type"].as_str() == Some(node_type))
        .unwrap()
}

#[test]
fn artifact_land_apply_can_use_native_ipv4_transport() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact publish"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-publish/backend");
    append_line(&backend_feature.join("app.txt"), "artifact land change");
    knit(
        &workspace,
        ["commit", "--all", "-m", "Artifact land change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let api_base = spawn_fake_github_api(&fake_gh_dir);

    let artifact = workspace.join(".knit/bundles/artifact-publish.bundle.json");
    let mut artifact_payload: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    artifact_payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    artifact_payload["publications"] = json!([
        {
            "repoId": "backend",
            "provider": "github",
            "kind": "pull_request",
            "number": 101,
            "url": "https://github.com/acme/backend/pull/101",
            "baseBranch": "main",
            "headBranch": "knit/artifact-publish",
            "state": "OPEN",
            "title": "artifact publish (backend)",
            "updatedAt": "2026-06-06T00:00:00.000Z"
        }
    ]);
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&artifact_payload).unwrap(),
    )
    .unwrap();

    let out = root.join("artifact-land.out.bundle.json");
    let landed = knit_with_fake_gh_env(
        &root,
        vec![
            "land".to_string(),
            "--target".to_string(),
            "staging".to_string(),
            "apply".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
            // `staging` is not the artifact's recorded base, so without this
            // the landing would be intermediate and merge the branch instead
            // of the review. This test exercises the review path's transport.
            "--terminal".to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &[
            ("GH_TOKEN", "gho_fake_token"),
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
            ("KNIT_GITHUB_API_BASE", api_base.as_str()),
        ],
    );
    assert!(landed.contains("checks backend"), "{landed}");
    assert!(
        landed.contains("retargeted backend PR #101 main -> staging"),
        "{landed}"
    );
    assert!(landed.contains("merged backend"), "{landed}");
    assert!(!fake_gh_dir.join("merge-order.txt").exists());
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("api.authorization"))
            .unwrap()
            .trim(),
        "Bearer gho_fake_token"
    );

    let payload: Value = serde_json::from_str(
        &fs::read_to_string(fake_gh_dir.join("api-backend-merge.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(payload["merge_method"].as_str(), Some("merge"));
    assert_eq!(payload["sha"].as_str(), Some("backend-head"));
    let edit_payload: Value = serde_json::from_str(
        &fs::read_to_string(fake_gh_dir.join("api-backend-edit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(edit_payload["base"].as_str(), Some("staging"));

    let landed_bundle: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        landed_bundle["publications"][0]["state"].as_str(),
        Some("MERGED")
    );
    assert_eq!(
        landed_bundle["publications"][0]["baseBranch"].as_str(),
        Some("staging")
    );
    assert!(landed_bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["type"].as_str() == Some("feature.landed")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_land_apply_accepts_a_server_resolved_lane_map() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact lane"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let artifact = workspace.join(".knit/bundles/artifact-lane.bundle.json");
    let mut payload: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    payload["publications"] = json!([{
        "repoId": "backend",
        "provider": "github",
        "kind": "pull_request",
        "number": 101,
        "url": "https://github.com/acme/backend/pull/101",
        "baseBranch": "main",
        "headBranch": "knit/artifact-lane",
        "state": "OPEN",
        "title": "artifact lane (backend)",
        "updatedAt": "2026-06-06T00:00:00.000Z"
    }]);
    fs::write(&artifact, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let api_base = spawn_fake_github_api(&fake_gh_dir);
    let out = root.join("artifact-lane.out.bundle.json");
    let landed = knit_with_fake_gh_env(
        &root,
        vec![
            "land".into(),
            "--lane".into(),
            "production".into(),
            "--repo-target".into(),
            "backend=stable".into(),
            "apply".into(),
            "--from-artifact".into(),
            artifact.to_string_lossy().to_string(),
            "--out".into(),
            out.to_string_lossy().to_string(),
            // A host holding the project metadata states the lifecycle rather
            // than letting Knit infer it. Terminal is what makes this landing
            // merge the review, which is what this test is about.
            "--terminal".into(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &[
            ("GH_TOKEN", "gho_fake_token"),
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
            ("KNIT_GITHUB_API_BASE", api_base.as_str()),
        ],
    );
    assert!(
        landed.contains("retargeted backend PR #101 main -> stable"),
        "{landed}"
    );
    let landed_payload: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(landed_payload["publications"][0]["baseBranch"], "stable");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_plan_and_apply_merges_recorded_publications_with_fake_gh() {
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
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend land",
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"),
        "frontend land",
    );
    knit(&workspace, ["commit", "--all", "-m", "Landing change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let missing_plan =
        knit_fails_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(missing_plan.contains("No land plan found"));

    let plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(plan.contains("Land plan"));
    assert!(plan.contains("merge-backend"));
    assert!(plan.contains("merge-frontend"));
    assert!(plan.contains("knit land apply"));
    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    assert!(plan_path.exists());
    let generated_plan: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    let steps = generated_plan["steps"].as_array().unwrap();
    assert_eq!(steps[0]["method"].as_str(), Some("merge"));
    assert_eq!(steps[1]["method"].as_str(), Some("merge"));
    assert!(!fake_gh_dir.join("merge-order.txt").exists());

    let existing_plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(existing_plan.contains("Land plan"));
    assert!(!fake_gh_dir.join("merge-order.txt").exists());

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("Feature landed"));
    assert!(
        apply.contains("landed venue-capacity; removed 2 worktree(s)"),
        "{apply}"
    );
    assert!(!workspace
        .join(".knit/worktrees/venue-capacity/backend")
        .exists());
    assert!(!workspace
        .join(".knit/worktrees/venue-capacity/frontend")
        .exists());
    assert!(
        git(&backend, ["branch", "--list", "knit/venue-capacity"]).contains("knit/venue-capacity")
    );
    assert!(
        git(&frontend, ["branch", "--list", "knit/venue-capacity"]).contains("knit/venue-capacity")
    );
    // This plan sets no repoOrder, so merges share a wave and run in parallel;
    // their relative order is unspecified, so compare as a set.
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    let mut order_lines = order.lines().collect::<Vec<_>>();
    order_lines.sort_unstable();
    assert_eq!(order_lines, vec!["backend", "frontend"]);
    let methods = fs::read_to_string(fake_gh_dir.join("merge-methods.txt")).unwrap();
    let mut method_lines = methods.lines().collect::<Vec<_>>();
    method_lines.sort_unstable();
    assert_eq!(method_lines, vec!["backend --merge", "frontend --merge"]);

    let bundle = read_bundle(&workspace);
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    let archive = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(archive["type"].as_str(), Some("feature.archived"));
    assert_eq!(archive["message"].as_str(), Some("landed"));
    assert_eq!(
        bundle["headNodeId"].as_str(),
        Some(archive["id"].as_str().unwrap())
    );
    let landed = latest_node_of_type(&bundle, "feature.landed");
    assert_eq!(landed["provider"].as_str(), Some("github"));
    assert_eq!(landed["repoIds"].as_array().unwrap().len(), 2);
    assert_eq!(landed["publicationUrls"].as_array().unwrap().len(), 2);
    let landed_node_id = landed["id"].as_str().unwrap().to_string();
    assert!(workspace.join(".knit/land-runs").exists());
    assert!(knit(
        &workspace,
        ["--bundle", "venue-capacity", "bundle", "validate"]
    )
    .contains("Bundle valid"));
    let archived_status = knit(&workspace, ["--bundle", "venue-capacity", "status"]);
    assert!(
        archived_status.contains("State: archived"),
        "{archived_status}"
    );
    assert!(!archived_status.contains("not landed"), "{archived_status}");
    assert!(knit(&workspace, ["--bundle", "venue-capacity", "log", "-1"]).contains("landed"));
    let sync_error = knit_fails(
        &workspace,
        ["--bundle", "venue-capacity", "sync", "push", "--bundles"],
    );
    assert!(sync_error.contains("No sync remote configured"));

    let revert_plan = knit_with_fake_gh(
        &workspace,
        ["--bundle", "venue-capacity", "revert", "HEAD"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(revert_plan.contains("Revert plan"), "{revert_plan}");
    assert!(revert_plan.contains("Provider: github"), "{revert_plan}");
    assert!(revert_plan.contains("prRevert"), "{revert_plan}");
    assert!(
        revert_plan.contains("https://github.com/acme/backend/pull/101"),
        "{revert_plan}"
    );
    assert!(
        revert_plan.contains("https://github.com/acme/frontend/pull/202"),
        "{revert_plan}"
    );

    let revert_apply = knit_with_fake_gh(
        &workspace,
        ["--bundle", "venue-capacity", "revert", "HEAD", "--apply"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        revert_apply.contains("Recorded PR revert group"),
        "{revert_apply}"
    );
    let revert_order = fs::read_to_string(fake_gh_dir.join("revert-order.txt")).unwrap();
    let mut revert_order_lines = revert_order.lines().collect::<Vec<_>>();
    revert_order_lines.sort_unstable();
    assert_eq!(revert_order_lines, vec!["backend", "frontend"]);
    let backend_revert_body = fs::read_to_string(fake_gh_dir.join("revert-backend.md")).unwrap();
    assert!(
        backend_revert_body.contains(&format!("Knit-Reverts: {landed_node_id}")),
        "{backend_revert_body}"
    );

    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("pr.revert"));
    assert_eq!(
        latest["targetNodeId"].as_str(),
        Some(landed_node_id.as_str())
    );
    assert_eq!(latest["provider"].as_str(), Some("github"));
    assert_eq!(latest["publicationUrls"].as_array().unwrap().len(), 2);
    assert!(bundle["publications"]
        .as_array()
        .unwrap()
        .iter()
        .any(|publication| {
            publication["repoId"].as_str() == Some("backend")
                && publication["number"].as_u64() == Some(901)
                && publication["state"].as_str() == Some("OPEN")
        }));
    assert!(bundle["publications"]
        .as_array()
        .unwrap()
        .iter()
        .any(|publication| {
            publication["repoId"].as_str() == Some("frontend")
                && publication["number"].as_u64() == Some(902)
                && publication["state"].as_str() == Some("OPEN")
        }));
    assert!(knit(
        &workspace,
        ["--bundle", "venue-capacity", "bundle", "validate"]
    )
    .contains("Bundle valid"));
    assert!(knit(&workspace, ["--bundle", "venue-capacity", "log", "-1"]).contains("pr revert"));
    let show_revert = knit(&workspace, ["--bundle", "venue-capacity", "show", "HEAD"]);
    assert!(show_revert.contains("pr.revert"), "{show_revert}");
    assert!(show_revert.contains(&landed_node_id), "{show_revert}");
    assert!(
        show_revert.contains("https://github.com/acme/backend/pull/901"),
        "{show_revert}"
    );

    let mut stale_bundle = read_bundle(&workspace);
    stale_bundle["publications"] = json!([]);
    fs::write(
        workspace.join(".knit/bundles/venue-capacity.bundle.json"),
        format!("{}\n", serde_json::to_string_pretty(&stale_bundle).unwrap()),
    )
    .unwrap();
    let stale_status = knit_with_fake_gh(
        &workspace,
        ["--bundle", "venue-capacity", "land", "status"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(!stale_status.contains("publication missing"));
    assert!(stale_status.contains("backend checkout missing"));
    assert!(stale_status.contains("frontend checkout missing"));

    fs::remove_dir_all(root).unwrap();
}

/// Create a two-repo bundle and publish its PRs through the fake `gh`, returning
/// the workspace plus the fake-gh paths so tests can toggle PR state via markers.
#[test]
fn land_apply_skips_already_merged_pr() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);

    // backend is already merged with no prior run recorded; a fresh land apply
    // must treat it as a satisfied step, not bail with "expected OPEN".
    fs::write(fake_gh_dir.join("merged-backend"), "").unwrap();
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("Feature landed"), "{apply}");
    // Only frontend should be merged; backend was skipped as already merged.
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(
        order.lines().collect::<Vec<_>>(),
        vec!["frontend"],
        "{order}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_keep_worktrees_archives_but_preserves_generated_checkouts() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote", "--keep-worktrees"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        apply.contains("landed venue-capacity; kept generated worktrees"),
        "{apply}"
    );
    assert!(workspace
        .join(".knit/worktrees/venue-capacity/backend")
        .exists());
    assert!(workspace
        .join(".knit/worktrees/venue-capacity/frontend")
        .exists());

    let bundle = read_bundle(&workspace);
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("feature.archived"));
    assert!(bundle["repos"]
        .as_array()
        .unwrap()
        .iter()
        .all(|repo| repo["worktreePath"].as_str().is_some()));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_check_reports_pr_readiness() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);

    // Both PRs open, clean, no required checks -> ready.
    let check = knit_with_fake_gh(&workspace, ["land", "check"], &fake_bin, &fake_gh_dir);
    assert!(check.contains("Readiness:"), "{check}");
    assert!(check.contains("backend"), "{check}");
    assert!(check.contains("frontend"), "{check}");
    assert!(check.contains("ready"), "{check}");

    // backend merged, frontend conflicting -> distinct verdicts + update hint.
    fs::write(fake_gh_dir.join("merged-backend"), "").unwrap();
    fs::write(fake_gh_dir.join("conflict-frontend"), "").unwrap();
    let check2 = knit_with_fake_gh(&workspace, ["land", "check"], &fake_bin, &fake_gh_dir);
    assert!(check2.contains("already landed"), "{check2}");
    assert!(check2.contains("conflict"), "{check2}");
    assert!(check2.contains("knit land update"), "{check2}");

    // `publish status --live` surfaces the same readiness columns.
    let live = knit_with_fake_gh(
        &workspace,
        ["publish", "status", "--live"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(live.contains("verdict"), "{live}");
    assert!(live.contains("conflict"), "{live}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_conflict_points_to_land_update() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);

    fs::write(fake_gh_dir.join("conflict-backend"), "").unwrap();
    let error = knit_fails_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(error.contains("merge conflicts"), "{error}");
    assert!(error.contains("knit land update"), "{error}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_landing_template_orders_merges_and_runs_deploy_from_base_checkout() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, backend_collaborator) = init_remote_repo(&root, "backend");
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

    fs::write(backend_collaborator.join("base.txt"), "ready for deploy\n").unwrap();
    git(&backend_collaborator, ["add", "base.txt"]);
    git(
        &backend_collaborator,
        ["commit", "-m", "Deploy base update"],
    );
    git(&backend_collaborator, ["push", "origin", "main"]);

    let deploy_pwd = root.join("deploy-pwd.txt");
    let deploy_branch = root.join("deploy-branch.txt");
    let deploy_ready = root.join("deploy-ready");
    let deploy_script = format!(
        "pwd > '{}' && git rev-parse --abbrev-ref HEAD > '{}' && test -f base.txt && test -f '{}'",
        deploy_pwd.display(),
        deploy_branch.display(),
        deploy_ready.display()
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "merge": {
            "repoOrder": ["frontend", "backend"],
            "method": "merge",
            "waitForChecks": true,
            "requiredChecksOnly": true,
            "deleteBranch": false
        },
        "deployments": [
            {
                "id": "deploy-backend",
                "repoId": "backend",
                "checkout": { "branch": "main", "remote": "origin", "update": "pull" },
                "timeoutSeconds": 120,
                "command": ["sh", "-c", deploy_script]
            },
            {
                "id": "deploy-frontend",
                "repoId": "frontend",
                "mode": "push"
            }
        ]
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend project landing",
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"),
        "frontend project landing",
    );
    knit(
        &workspace,
        ["commit", "--all", "-m", "Project landing change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);
    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    let plan: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    assert_eq!(plan["sourceProjectId"].as_str(), Some("demo"));
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(steps[0]["id"].as_str(), Some("merge-frontend"));
    assert_eq!(steps[1]["id"].as_str(), Some("merge-backend"));
    assert_eq!(steps[2]["type"].as_str(), Some("deploy"));
    assert_eq!(steps[2]["id"].as_str(), Some("deploy-backend"));
    assert_eq!(steps[2]["timeoutSeconds"].as_u64(), Some(120));
    assert_eq!(
        steps[2]["needs"].as_array().unwrap()[0].as_str(),
        Some("merge-backend")
    );
    assert_eq!(steps[3]["id"].as_str(), Some("deploy-frontend"));
    assert!(steps[3].get("timeoutSeconds").is_none());
    assert_eq!(
        steps[3]["needs"].as_array().unwrap()[0].as_str(),
        Some("merge-frontend")
    );

    // Reproduce a checkout left by an earlier landing. The source clone has
    // not fetched the collaborator's base update yet, so apply must fetch it
    // and move this existing worktree to the fetched commit before deploying.
    let managed_checkout = workspace.join(".knit/land-worktrees/venue-capacity/backend/main");
    fs::create_dir_all(managed_checkout.parent().unwrap()).unwrap();
    git(
        &backend,
        [
            "worktree",
            "add",
            "--detach",
            managed_checkout.to_str().unwrap(),
            "HEAD",
        ],
    );

    let apply = knit_fails_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(apply.contains("deploy-backend"));
    fs::write(&deploy_ready, "ready\n").unwrap();
    let resume = knit_with_fake_gh(&workspace, ["land", "resume"], &fake_bin, &fake_gh_dir);
    assert!(resume.contains("Feature landed"));
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(
        order.lines().collect::<Vec<_>>(),
        vec!["frontend", "backend"]
    );
    assert!(fs::read_to_string(&deploy_pwd)
        .unwrap()
        .contains(".knit/land-worktrees/venue-capacity/backend/main"));
    assert_eq!(fs::read_to_string(&deploy_branch).unwrap().trim(), "HEAD");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn alternate_base_plan_selects_declared_target_deployments() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    git(&backend, ["branch", "staging", "main"]);
    git(&backend, ["push", "origin", "staging"]);
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let deployed_target = root.join("deployed-target.txt");
    let deploy_staging = format!(
        "printf '%s' \"$KNIT_LAND_TARGET_BRANCH\" > '{}'",
        deployed_target.display()
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "deployments": [{
            "id": "deploy-production",
            "repoId": "backend",
            "command": ["deploy-production"]
        }],
        "targets": {
            "staging": {
                "deployments": [{
                    "id": "deploy-staging",
                    "repoId": "backend",
                    "command": ["sh", "-c", deploy_staging]
                }]
            }
        }
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "staging target"]);
    let feature = workspace.join(".knit/worktrees/staging-target/backend");
    append_line(&feature.join("app.txt"), "staging change");
    knit(&workspace, ["commit", "--all", "-m", "Staging change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let output = knit_with_fake_gh(
        &workspace,
        ["land", "--target", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(output.contains("backend -> staging"), "{output}");
    assert!(
        output.contains("matching `landing.targets.<branch>` deployment steps are included"),
        "{output}"
    );
    // `staging` is not backend's configured base, so this target is an
    // intermediate destination: reached by branch merge, reviews untouched.
    assert!(
        output.contains("feature branches into the destination; review objects stay open"),
        "{output}"
    );
    let plan_path = workspace.join(".knit/land-plans/staging-target.land.json");
    let plan: Value = serde_json::from_str(&fs::read_to_string(plan_path).unwrap()).unwrap();
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(plan["targetBranch"].as_str(), Some("staging"));
    assert_eq!(plan["terminal"].as_bool(), Some(false));
    assert_eq!(steps[0]["id"].as_str(), Some("merge-backend"));
    assert_eq!(steps[0]["type"].as_str(), Some("merge_branch"));
    assert_eq!(steps[0]["targetBranch"].as_str(), Some("staging"));
    assert_eq!(steps[1]["id"].as_str(), Some("deploy-staging"));
    assert_eq!(
        steps[1]["env"]["KNIT_LAND_TARGET_BRANCH"].as_str(),
        Some("staging")
    );
    assert!(steps
        .iter()
        .all(|step| step["id"].as_str() != Some("deploy-production")));

    let mismatched = knit_fails_with_fake_gh(
        &workspace,
        ["land", "apply", "--target", "preproduction"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        mismatched.contains("Land plan targets staging"),
        "{mismatched}"
    );
    assert!(!fake_gh_dir.join("retarget-order.txt").exists());

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--target", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("deploy-staging"), "{apply}");
    assert!(apply.contains("bundle stays open"), "{apply}");
    assert_eq!(fs::read_to_string(&deployed_target).unwrap(), "staging");
    // The environment got the work by branch merge; the review was neither
    // retargeted nor merged, so it still points at the terminal destination.
    assert!(!fake_gh_dir.join("retarget-order.txt").exists());
    assert!(!fake_gh_dir.join("merged-backend").exists());
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("create-backend.base"))
            .unwrap()
            .trim(),
        "main"
    );
    let staging = git(&backend_remote, ["log", "--oneline", "staging"]);
    assert!(staging.contains("Staging change"), "{staging}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn named_lane_resolves_per_repo_targets_and_lane_deployments() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
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
    let deployed_lane = root.join("deployed-lane.txt");
    let deploy_lane = format!(
        "printf '%s:%s' \"$KNIT_LAND_LANE\" \"$KNIT_LAND_TARGET_BRANCH\" > '{}'",
        deployed_lane.display()
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "lanes": {
            "production": {
                "terminal": true,
                "branches": {
                    "backend": "stable",
                    "frontend": "master"
                },
                "deployments": [{
                    "id": "deploy-production",
                    "repoId": "backend",
                    "command": ["sh", "-c", deploy_lane]
                }]
            }
        }
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "production lane"]);
    for repo_id in ["backend", "frontend"] {
        append_line(
            &workspace
                .join(".knit/worktrees/production-lane")
                .join(repo_id)
                .join("app.txt"),
            "production change",
        );
    }
    knit(&workspace, ["commit", "--all", "-m", "Production change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let output = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "production"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(output.contains("Lane: production"), "{output}");
    assert!(output.contains("backend -> stable"), "{output}");
    assert!(output.contains("frontend -> master"), "{output}");

    let plan_path = workspace.join(".knit/land-plans/production-lane.land.json");
    let plan: Value = serde_json::from_str(&fs::read_to_string(plan_path).unwrap()).unwrap();
    assert_eq!(plan["lane"].as_str(), Some("production"));
    assert!(plan.get("targetBranch").is_none());
    assert_eq!(plan["targetBranches"]["backend"].as_str(), Some("stable"));
    assert_eq!(plan["targetBranches"]["frontend"].as_str(), Some("master"));
    let deployment = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["id"] == "deploy-production")
        .unwrap();
    assert_eq!(deployment["env"]["KNIT_LAND_LANE"], "production");
    assert_eq!(deployment["env"]["KNIT_LAND_TARGET_BRANCH"], "stable");

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "production", "apply"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("retargeted backend"), "{apply}");
    assert!(apply.contains("retargeted frontend"), "{apply}");
    assert_eq!(
        fs::read_to_string(&deployed_lane).unwrap(),
        "production:stable"
    );
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("create-backend.base"))
            .unwrap()
            .trim(),
        "stable"
    );
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("create-frontend.base"))
            .unwrap()
            .trim(),
        "master"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn alternate_base_plan_without_declared_target_keeps_deployment_explicit() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "deployments": [{
            "id": "deploy-production",
            "repoId": "backend",
            "command": ["deploy-production"]
        }],
        "targets": {
            "staging": {
                "deployments": [{
                    "id": "deploy-staging",
                    "repoId": "backend",
                    "command": ["deploy-staging"]
                }]
            }
        }
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "preproduction target"]);
    let feature = workspace.join(".knit/worktrees/preproduction-target/backend");
    append_line(&feature.join("app.txt"), "preproduction change");
    knit(
        &workspace,
        ["commit", "--all", "-m", "Preproduction change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let output = knit_with_fake_gh(
        &workspace,
        ["land", "--target", "preproduction"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(output.contains("backend -> preproduction"), "{output}");
    assert!(
        output.contains("no deployment steps matched these branches"),
        "{output}"
    );
    let plan_path = workspace.join(".knit/land-plans/preproduction-target.land.json");
    let plan: Value = serde_json::from_str(&fs::read_to_string(plan_path).unwrap()).unwrap();
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["id"].as_str(), Some("merge-backend"));
    // An undeclared, non-base target is an intermediate destination, so the
    // one step is a branch merge into it.
    assert_eq!(steps[0]["type"].as_str(), Some("merge_branch"));
    assert_eq!(steps[0]["targetBranch"].as_str(), Some("preproduction"));
    assert_eq!(plan["terminal"].as_bool(), Some(false));

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn command_deployment_timeout_is_recorded_and_terminates_descendants() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    let child_pid_path = root.join("deploy-child.pid");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "deployments": [{
            "id": "deploy-backend",
            "repoId": "backend",
            "timeoutSeconds": 1,
            "command": [
                "sh",
                "-c",
                "printf 'deploy started\\n'; sleep 20 & echo $! > \"$KNIT_TEST_PID_FILE\"; wait"
            ],
            "env": { "KNIT_TEST_PID_FILE": child_pid_path }
        }]
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "timed deploy"]);
    knit(&workspace, ["land", "plan"]);
    let plan_path = workspace.join(".knit/land-plans/timed-deploy.land.json");
    let plan: Value = serde_json::from_str(&fs::read_to_string(plan_path).unwrap()).unwrap();
    assert_eq!(plan["steps"][0]["timeoutSeconds"].as_u64(), Some(1));

    let failed = knit_fails(&workspace, ["land", "apply"]);
    assert!(failed.contains("deploy started"), "{failed}");
    assert!(failed.contains("timed out after 1 seconds"), "{failed}");

    let (_, run) = latest_land_run(&workspace);
    assert_eq!(run["status"].as_str(), Some("failed"));
    assert_eq!(run["steps"][0]["status"].as_str(), Some("failed"));
    assert!(run["steps"][0]["stdout"]
        .as_str()
        .unwrap()
        .contains("deploy started"));
    assert!(run["steps"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("timed out after 1 seconds"));

    let child_pid = fs::read_to_string(&child_pid_path)
        .unwrap()
        .trim()
        .to_string();
    let child_alive = std::process::Command::new("kill")
        .args(["-0", &child_pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(
        !child_alive,
        "timed-out deployment child {child_pid} survived"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn terminating_knit_cancels_deployment_tree_and_records_failure() {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    let child_pid_path = root.join("cancelled-deploy-child.pid");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "deployments": [{
            "id": "deploy-backend",
            "repoId": "backend",
            "timeoutSeconds": 60,
            "command": [
                "sh",
                "-c",
                "printf 'deploy started\\n'; sleep 20 & echo $! > \"$KNIT_TEST_PID_FILE\"; wait"
            ],
            "env": { "KNIT_TEST_PID_FILE": child_pid_path }
        }]
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "cancelled deploy"]);
    knit(&workspace, ["land", "plan"]);
    let child = Command::new(env!("CARGO_BIN_EXE_knit"))
        .args(["land", "apply"])
        .current_dir(&workspace)
        .env("KNIT_HOME", isolated_knit_home())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let marker_deadline = Instant::now() + Duration::from_secs(5);
    while !child_pid_path.exists() && Instant::now() < marker_deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(child_pid_path.exists(), "deployment command did not start");
    // SAFETY: the pid is from the live Child handle and SIGTERM is handled by
    // Knit to cancel registered landing subprocesses before returning.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success());
    assert!(combined.contains("deploy `sh -c"), "{combined}");
    assert!(combined.contains("cancelled"), "{combined}");

    let (_, run) = latest_land_run(&workspace);
    assert_eq!(run["status"].as_str(), Some("failed"));
    assert_eq!(run["steps"][0]["status"].as_str(), Some("failed"));
    assert!(run["steps"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("cancelled"));

    let descendant_pid = fs::read_to_string(&child_pid_path)
        .unwrap()
        .trim()
        .to_string();
    let descendant_alive = Command::new("kill")
        .args(["-0", &descendant_pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(
        !descendant_alive,
        "cancelled deployment child {descendant_pid} survived"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_update_merges_base_and_records_explicit_node() {
    let root = unique_temp_dir();
    let (backend_remote, backend, backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    let backend_feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    append_line(&backend_feature.join("app.txt"), "backend feature update");
    knit(&workspace, ["commit", "--all", "-m", "Feature update"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    fs::write(
        backend_collaborator.join("base.txt"),
        "base branch update\n",
    )
    .unwrap();
    git(&backend_collaborator, ["add", "base.txt"]);
    git(
        &backend_collaborator,
        ["commit", "-m", "Base branch update"],
    );
    git(&backend_collaborator, ["push", "origin", "main"]);

    let update = knit_with_fake_gh(
        &workspace,
        ["land", "update", "--push"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(update.contains("backend"));
    assert!(update.contains("updated"));
    assert!(update.contains("pushed"));

    let local_head = git(&backend_feature, ["rev-parse", "HEAD"]);
    assert_eq!(
        git(
            &backend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        local_head
    );

    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("land.update"));
    assert_eq!(latest["provider"].as_str(), Some("github"));
    let repo_changes = latest["repoChanges"].as_array().unwrap();
    assert_eq!(repo_changes.len(), 1);
    assert_eq!(repo_changes[0]["repoId"].as_str(), Some("backend"));
    assert_eq!(
        repo_changes[0]["afterSha"].as_str(),
        Some(local_head.trim())
    );
    assert_eq!(
        bundle["repos"][0]["headSha"].as_str(),
        Some(local_head.trim())
    );

    let log = knit(&workspace, ["log", "-1"]);
    assert!(log.contains("updated from base"));
    let show = knit(&workspace, ["show", "HEAD"]);
    assert!(show.contains("land.update"));
    assert!(show.contains("Base branch update"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_resume_skips_succeeded_steps_and_retries_failed_run_steps() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend land resume",
    );
    knit(
        &workspace,
        ["commit", "--all", "-m", "Landing resume change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);

    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    let mut plan: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    let failing_deploy_command = concat!(
        "if test \"$DEPLOY_OK\" = \"yes\" && test -f deploy-ok; then exit 0; fi; ",
        "printf 'deploy stdout context\\n'; ",
        "printf 'fatal: deploy-ok marker is missing\\n' >&2; ",
        "exit 42"
    );
    plan["steps"].as_array_mut().unwrap().push(json!({
        "id": "deploy",
        "type": "run",
        "cwd": ".",
        "command": ["sh", "-c", failing_deploy_command],
        "env": { "DEPLOY_OK": "yes" },
        "needs": ["merge-backend"]
    }));
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    let failed = knit_fails_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(failed.contains("stopped at step deploy"));
    assert!(failed.contains("deploy failed output excerpt"), "{failed}");
    assert!(
        failed.contains("fatal: deploy-ok marker is missing"),
        "{failed}"
    );
    assert!(failed.contains("Full output:"), "{failed}");
    let bundle_after_failure = read_bundle(&workspace);
    assert_ne!(
        bundle_after_failure["nodes"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["type"]
            .as_str(),
        Some("feature.landed")
    );
    assert_ne!(bundle_after_failure["state"].as_str(), Some("archived"));
    assert!(workspace
        .join(".knit/worktrees/venue-capacity/backend")
        .exists());

    fs::write(workspace.join("deploy-ok"), "ready\n").unwrap();
    let resumed = knit_with_fake_gh(&workspace, ["land", "resume"], &fake_bin, &fake_gh_dir);
    assert!(resumed.contains("Feature landed"));
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(order.lines().collect::<Vec<_>>(), vec!["backend"]);

    // A resumed terminal landing has to finish the bundle exactly as an
    // applied one does. It used to merge everything and then leave the bundle
    // open, unarchived and still active, so the forge said the work had landed
    // and the local ledger said it had not.
    let landed = read_bundle(&workspace);
    assert_eq!(landed["state"].as_str(), Some("archived"));
    assert_eq!(
        latest_node_of_type(&landed, "feature.landed")["landing"]["terminal"].as_bool(),
        Some(true)
    );
    latest_node_of_type(&landed, "feature.archived");
    assert!(!workspace
        .join(".knit/worktrees/venue-capacity/backend")
        .exists());

    let status = knit_with_fake_gh(
        &workspace,
        ["--bundle", "venue-capacity", "land", "status"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(status.contains("succeeded"));
    assert!(status.contains("deploy"));

    fs::remove_dir_all(root).unwrap();
}

/// Make the venue-capacity plan land sequentially with a failing gate between
/// the two merges: merge-backend, then a `run` step that fails, then
/// merge-frontend. Applying it merges backend only and leaves a failed run.
fn write_half_failing_plan(workspace: &Path, on_failure: Option<&str>) {
    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    let mut plan: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    if let Some(on_failure) = on_failure {
        plan["onFailure"] = json!(on_failure);
    }
    let steps = plan["steps"].as_array_mut().unwrap();
    for step in steps.iter_mut() {
        if step["id"].as_str() == Some("merge-frontend") {
            step["needs"] = json!(["gate"]);
        }
    }
    steps.push(json!({
        "id": "gate",
        "type": "run",
        "cwd": ".",
        "command": ["sh", "-c", "false"],
        "needs": ["merge-backend"]
    }));
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
}

fn latest_land_run(workspace: &Path) -> (std::path::PathBuf, Value) {
    let run_dir = workspace.join(".knit/land-runs");
    let mut paths: Vec<_> = fs::read_dir(&run_dir)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    let path = paths.pop().unwrap();
    let run: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    (path, run)
}

/// A run records the steps of the plan it started from. Regenerating the plan
/// in between (`knit land plan --force` after more work, say) gives it a
/// different step list; resuming used to panic on the mismatch instead of
/// saying what happened.
#[test]
fn land_resume_refuses_a_run_whose_plan_was_regenerated() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);
    write_half_failing_plan(&workspace, None);

    let failed = knit_fails_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(failed.contains("stopped at step gate"), "{failed}");

    // The plan is rewritten underneath the run: the gate it recorded is gone,
    // and a step it never recorded is appended.
    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    let mut plan: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    let steps = plan["steps"].as_array_mut().unwrap();
    steps.retain(|step| step["id"].as_str() != Some("gate"));
    for step in steps.iter_mut() {
        if step["id"].as_str() == Some("merge-frontend") {
            step["needs"] = json!(["merge-backend"]);
        }
    }
    steps.push(json!({
        "id": "smoke",
        "type": "run",
        "cwd": ".",
        "command": ["sh", "-c", "true"],
        "needs": ["merge-frontend"]
    }));
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    let resume = knit_fails_with_fake_gh(&workspace, ["land", "resume"], &fake_bin, &fake_gh_dir);
    assert!(
        resume.contains("recorded against a different plan"),
        "{resume}"
    );
    assert!(resume.contains("smoke"), "{resume}");
    assert!(resume.contains("gate"), "{resume}");
    assert!(resume.contains("knit land apply"), "{resume}");
    assert!(!resume.contains("panicked"), "{resume}");

    // Nothing moved: backend merged during apply, frontend still has not, and
    // the run was left as it was.
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(order.lines().collect::<Vec<_>>(), vec!["backend"]);
    let (_, run) = latest_land_run(&workspace);
    assert_eq!(run["status"].as_str(), Some("failed"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_rollback_creates_revert_prs_for_merged_steps_of_failed_run() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);
    write_half_failing_plan(&workspace, None);

    let failed = knit_fails_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(failed.contains("stopped at step gate"), "{failed}");
    assert!(failed.contains("knit land rollback"), "{failed}");
    // backend merged before the gate failed; frontend never merged.
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(order.lines().collect::<Vec<_>>(), vec!["backend"]);
    let (_, run) = latest_land_run(&workspace);
    let run_id = run["id"].as_str().unwrap().to_string();
    assert_eq!(run["status"].as_str(), Some("failed"));

    // Preview shows the merged step and creates nothing.
    let preview = knit_with_fake_gh(&workspace, ["land", "rollback"], &fake_bin, &fake_gh_dir);
    assert!(preview.contains("Land rollback"), "{preview}");
    assert!(
        preview.contains("https://github.com/acme/backend/pull/101"),
        "{preview}"
    );
    assert!(preview.contains("MERGED"), "{preview}");
    assert!(preview.contains("knit land rollback --apply"), "{preview}");
    assert!(!preview.contains("frontend"), "{preview}");
    assert!(!fake_gh_dir.join("revert-order.txt").exists());

    let applied = knit_with_fake_gh(
        &workspace,
        ["land", "rollback", "--apply"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(applied.contains("Recorded PR revert group"), "{applied}");
    assert!(applied.contains("Rolled back"), "{applied}");
    // Only the merged backend PR is reverted.
    let revert_order = fs::read_to_string(fake_gh_dir.join("revert-order.txt")).unwrap();
    assert_eq!(revert_order.lines().collect::<Vec<_>>(), vec!["backend"]);

    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("pr.revert"));
    assert_eq!(latest["targetNodeId"].as_str(), Some(run_id.as_str()));
    assert_eq!(latest["provider"].as_str(), Some("github"));
    assert!(bundle["publications"]
        .as_array()
        .unwrap()
        .iter()
        .any(|publication| {
            publication["repoId"].as_str() == Some("backend")
                && publication["number"].as_u64() == Some(901)
                && publication["state"].as_str() == Some("OPEN")
        }));
    assert!(knit(&workspace, ["bundle", "validate"]).contains("Bundle valid"));

    let (_, run) = latest_land_run(&workspace);
    assert!(run["rolledBackAt"].as_str().is_some());

    // A rolled-back run can be neither resumed nor rolled back again.
    let resume = knit_fails_with_fake_gh(&workspace, ["land", "resume"], &fake_bin, &fake_gh_dir);
    assert!(resume.contains("was rolled back"), "{resume}");
    let again = knit_fails_with_fake_gh(&workspace, ["land", "rollback"], &fake_bin, &fake_gh_dir);
    assert!(again.contains("already rolled back"), "{again}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_on_failure_rollback_reverts_merged_steps_automatically() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);
    write_half_failing_plan(&workspace, Some("rollback"));

    let failed = knit_fails_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(failed.contains("stopped at step gate"), "{failed}");
    assert!(failed.contains("rolling back"), "{failed}");
    assert!(failed.contains("revert group"), "{failed}");

    let revert_order = fs::read_to_string(fake_gh_dir.join("revert-order.txt")).unwrap();
    assert_eq!(revert_order.lines().collect::<Vec<_>>(), vec!["backend"]);
    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("pr.revert"));
    let (_, run) = latest_land_run(&workspace);
    assert!(run["rolledBackAt"].as_str().is_some());

    let resume = knit_fails_with_fake_gh(&workspace, ["land", "resume"], &fake_bin, &fake_gh_dir);
    assert!(resume.contains("was rolled back"), "{resume}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_refuses_draft_publications() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend draft land",
    );
    knit(
        &workspace,
        ["commit", "--all", "-m", "Draft landing change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);

    let failed = knit_fails_with_fake_gh_env(
        &workspace,
        ["land", "apply"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_DRAFT", "1")],
    );
    assert!(failed.contains("is a draft"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_stops_when_required_checks_fail() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend check failure",
    );
    knit(
        &workspace,
        ["commit", "--all", "-m", "Check failure landing"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);

    let failed = knit_fails_with_fake_gh_env(
        &workspace,
        ["land", "apply"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_CHECKS_FAIL", "1")],
    );
    assert!(failed.contains("required checks failed: test"));
    assert!(!fake_gh_dir.join("merge-order.txt").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_treats_no_required_checks_as_ready() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "docs cleanup"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/docs-cleanup/backend/app.txt"),
        "docs cleanup landing",
    );
    knit(&workspace, ["commit", "--all", "-m", "Docs cleanup"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    let plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(plan.contains("Land plan"));
    let plan_json: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/land-plans/docs-cleanup.land.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(plan_json["steps"][0]["waitForChecks"].as_bool(), Some(true));

    let status = knit_with_fake_gh_env(
        &workspace,
        ["--bundle", "docs-cleanup", "land", "status"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_NO_REQUIRED_CHECKS_ERROR", "1")],
    );
    assert!(status.contains("checks passed (no required checks)"));
    assert!(!status.contains("checks unavailable"));

    let apply = knit_with_fake_gh_env(
        &workspace,
        ["land", "apply"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_NO_REQUIRED_CHECKS_ERROR", "1")],
    );
    assert!(apply.contains("Feature landed"));
    let run_status = knit_with_fake_gh_env(
        &workspace,
        ["--bundle", "docs-cleanup", "land", "status"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_NO_REQUIRED_CHECKS_ERROR", "1")],
    );
    assert!(run_status.contains("checks passed (no required checks)"));
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(order.trim(), "backend");

    fs::remove_dir_all(root).unwrap();
}

/// Two published repos on `main` with a project landing template whose lanes
/// come from the caller, so each test can vary only the destination's declared
/// or inferred lifecycle.
fn publish_lane_bundle(
    root: &Path,
    bundle_title: &str,
    lanes: Value,
    environment_branches: &[&str],
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(root, "frontend");
    for checkout in [&backend, &frontend] {
        for branch in environment_branches {
            git(checkout, ["branch", branch, "main"]);
            git(checkout, ["push", "origin", branch]);
        }
    }
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

    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({ "provider": "github", "lanes": lanes });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", bundle_title]);
    let slug = bundle_title.replace(' ', "-");
    for repo_id in ["backend", "frontend"] {
        append_line(
            &workspace
                .join(".knit/worktrees")
                .join(&slug)
                .join(repo_id)
                .join("app.txt"),
            "lane change",
        );
    }
    knit(&workspace, ["commit", "--all", "-m", "Lane change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    (workspace, fake_bin, fake_gh_dir)
}

fn read_named_bundle(workspace: &Path, slug: &str) -> Value {
    let path = workspace.join(format!(".knit/bundles/{slug}.bundle.json"));
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn intermediate_lane_land_keeps_the_bundle_open() {
    let root = unique_temp_dir();
    let deployed = root.join("deployed-staging.txt");
    let deploy_staging = format!(
        "printf '%s:%s' \"$KNIT_LAND_LANE\" \"$KNIT_LAND_TARGET_BRANCH\" > '{}'",
        deployed.display()
    );
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "staging work",
        json!({
            "staging": {
                "defaultBranch": "staging",
                "deployments": [{
                    "id": "deploy-staging",
                    "repoId": "backend",
                    "command": ["sh", "-c", deploy_staging]
                }]
            }
        }),
        &["staging"],
    );

    let plan = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        plan.contains("stays open on success (intermediate destination)"),
        "{plan}"
    );

    assert!(
        plan.contains("feature branches into the destination"),
        "{plan}"
    );

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("bundle stays open"), "{apply}");

    // The environment was deployed to after its branches moved.
    assert_eq!(fs::read_to_string(&deployed).unwrap(), "staging:staging");

    // The environment got the work by branch merge, and the review object was
    // left alone: it still points at the destination that ends the bundle.
    assert!(!fake_gh_dir.join("retarget-order.txt").exists());
    for repo_id in ["backend", "frontend"] {
        let remote = root.join(format!("{repo_id}.git"));
        let staging = git(&remote, ["log", "--oneline", "staging"]);
        assert!(staging.contains("Lane change"), "{staging}");
        let main = git(&remote, ["log", "--oneline", "main"]);
        assert!(!main.contains("Lane change"), "{main}");
    }
    let publications = read_named_bundle(&workspace, "staging-work")["publications"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(publications.len(), 2);
    for publication in &publications {
        assert_eq!(publication["baseBranch"].as_str(), Some("main"));
        assert_eq!(publication["state"].as_str(), Some("OPEN"));
    }

    let bundle = read_named_bundle(&workspace, "staging-work");
    assert_eq!(bundle["state"].as_str(), Some("open"));
    assert!(
        !bundle["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["type"].as_str() == Some("feature.archived")),
        "{bundle}"
    );
    let landed = latest_node_of_type(&bundle, "feature.landed");
    assert_eq!(landed["landing"]["terminal"].as_bool(), Some(false));
    assert_eq!(landed["landing"]["lane"].as_str(), Some("staging"));

    // The bundle keeps everything it needs to reach its next destination.
    assert!(workspace
        .join(".knit/worktrees/staging-work/backend")
        .exists());
    assert!(workspace
        .join(".knit/worktrees/staging-work/frontend")
        .exists());
    let status = knit(&workspace, ["bundle"]);
    assert!(status.contains("staging-work"), "{status}");

    fs::remove_dir_all(root).unwrap();
}

/// A lane merge whose push is rejected (someone else moved the shared
/// environment branch, a hook said no) must leave the managed checkout on the
/// tip it was fetched at. It used to keep the merge commit, so `knit land
/// resume` refused the checkout for "local commits not in origin" -- a commit
/// Knit itself had made -- and the run could never be resumed.
#[cfg(unix)]
#[test]
fn a_lane_push_failure_undoes_the_merge_and_resume_pushes_it() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "staging work",
        json!({ "staging": { "defaultBranch": "staging" } }),
        &["staging"],
    );

    // Reject pushes into backend's origin while the marker exists.
    let marker = root.join("reject-push");
    fs::write(&marker, "reject\n").unwrap();
    let backend_remote = root.join("backend.git");
    let hook = backend_remote.join("hooks/pre-receive");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nif [ -f {} ]; then\n  echo 'staging moved under you' >&2\n  exit 1\nfi\nexit 0\n",
            shell_quote(&marker.to_string_lossy())
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let staging_before = git(&backend_remote, ["rev-parse", "staging"]);
    let feature_sha = git(
        &workspace.join(".knit/worktrees/staging-work/backend"),
        ["rev-parse", "HEAD"],
    );

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let failed = knit_fails_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        failed.contains("backend: failed to push origin/staging"),
        "{failed}"
    );
    assert!(failed.contains("staging moved under you"), "{failed}");
    assert!(failed.contains("The local merge was undone"), "{failed}");
    assert!(failed.contains("knit land resume"), "{failed}");

    // The rejected merge did not reach origin, and the managed checkout is
    // back on the tip it fetched, not one merge commit ahead of it.
    assert_eq!(
        git(&backend_remote, ["rev-parse", "staging"]),
        staging_before
    );
    let managed = workspace.join(".knit/merge-worktrees/staging/backend");
    assert!(managed.exists(), "{}", managed.display());
    assert_eq!(
        git(&managed, ["rev-parse", "HEAD"]),
        git(&managed, ["rev-parse", "origin/staging"])
    );
    assert_eq!(git(&managed, ["status", "--porcelain"]), "");

    // Let the push through and resume: the step merges again and pushes.
    // The lane is stored in the plan, so resume takes no `--lane`.
    fs::remove_file(&marker).unwrap();
    let resumed = knit_with_fake_gh(
        &workspace,
        ["land", "resume", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        resumed.contains("into staging") && resumed.contains("and pushed"),
        "{resumed}"
    );
    assert!(!resumed.contains("already contains"), "{resumed}");
    assert!(resumed.contains("bundle stays open"), "{resumed}");

    let staging_after = git(&backend_remote, ["rev-parse", "staging"]);
    assert_ne!(staging_after, staging_before);
    assert!(
        git_success(
            &backend_remote,
            ["merge-base", "--is-ancestor", feature_sha.trim(), "staging"]
        ),
        "origin/staging does not contain the feature tip {feature_sha}"
    );
    assert_eq!(git(&managed, ["rev-parse", "HEAD"]), staging_after);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lane_declared_terminal_archives_the_bundle() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "release work",
        json!({ "production": { "defaultBranch": "stable", "terminal": true } }),
        &[],
    );

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "production"],
        &fake_bin,
        &fake_gh_dir,
    );
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "production", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("landed release-work"), "{apply}");
    assert!(apply.contains("removed 2 worktree(s)"), "{apply}");

    let bundle = read_named_bundle(&workspace, "release-work");
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    let landed = latest_node_of_type(&bundle, "feature.landed");
    assert_eq!(landed["landing"]["terminal"].as_bool(), Some(true));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lane_on_configured_bases_is_terminal_without_declaring_it() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "base work",
        json!({ "production": { "defaultBranch": "main" } }),
        &[],
    );

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "production"],
        &fake_bin,
        &fake_gh_dir,
    );
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "production", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("landed base-work"), "{apply}");

    let bundle = read_named_bundle(&workspace, "base-work");
    assert_eq!(bundle["state"].as_str(), Some("archived"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tagging_is_refused_when_the_destination_is_not_terminal() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "tagged staging",
        json!({ "staging": { "defaultBranch": "staging" } }),
        &["staging"],
    );

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let failure = knit_fails_with_fake_gh(
        &workspace,
        [
            "land",
            "--lane",
            "staging",
            "apply",
            "--no-remote",
            "--tag",
            "verified",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(failure.contains("not a terminal destination"), "{failure}");

    // Refused before anything merged, so the bundle is untouched.
    let bundle = read_named_bundle(&workspace, "tagged-staging");
    assert_eq!(bundle["state"].as_str(), Some("open"));
    assert!(
        !bundle["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["type"].as_str() == Some("feature.landed")),
        "{bundle}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_lands_into_staging_then_production() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "two stop work",
        json!({
            "staging": { "defaultBranch": "staging" },
            "production": { "defaultBranch": "main" }
        }),
        &["staging"],
    );

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let staging = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(staging.contains("bundle stays open"), "{staging}");

    // The finished staging run is history: asking for the next environment
    // plans it instead of reporting the old run.
    let plan = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "production"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(plan.contains("Lane: production"), "{plan}");
    assert!(
        plan.contains("archived on success (terminal destination)"),
        "{plan}"
    );
    assert!(plan.contains("the recorded review objects"), "{plan}");

    let production = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "production", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(production.contains("landed two-stop-work"), "{production}");

    let bundle = read_named_bundle(&workspace, "two-stop-work");
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    let landings = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"].as_str() == Some("feature.landed"))
        .collect::<Vec<_>>();
    assert_eq!(landings.len(), 2);
    assert_eq!(landings[0]["landing"]["lane"].as_str(), Some("staging"));
    assert_eq!(landings[0]["landing"]["terminal"].as_bool(), Some(false));
    assert_eq!(landings[1]["landing"]["lane"].as_str(), Some("production"));
    assert_eq!(landings[1]["landing"]["terminal"].as_bool(), Some(true));

    fs::remove_dir_all(root).unwrap();
}

/// A finished run is an answer only while its plan still describes the
/// bundle. After more work is committed, asking for the same lane again is a
/// request to land that work, not to hear about the old run.
#[test]
fn landing_the_same_lane_again_after_new_work_plans_the_new_work() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "staging twice",
        json!({
            "staging": { "defaultBranch": "staging" },
            "production": { "defaultBranch": "main" }
        }),
        &["staging"],
    );

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let first = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(first.contains("bundle stays open"), "{first}");

    append_line(
        &workspace.join(".knit/worktrees/staging-twice/backend/app.txt"),
        "more staging work",
    );
    knit(&workspace, ["commit", "--all", "-m", "More staging work"]);

    let plan = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(plan.contains("Lane: staging"), "{plan}");
    assert!(plan.contains("stays open on success"), "{plan}");
    assert!(!plan.contains("Land run"), "{plan}");

    let again = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(again.contains("bundle stays open"), "{again}");
    let staging = git(&root.join("backend.git"), ["log", "--oneline", "staging"]);
    assert!(staging.contains("More staging work"), "{staging}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bare_land_after_a_lane_landing_plans_the_terminal_destination() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "bare after lane",
        json!({ "staging": { "defaultBranch": "staging" } }),
        &["staging"],
    );

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let staging = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(staging.contains("bundle stays open"), "{staging}");

    // A bare request means the recorded review bases. The finished staging
    // run is history for that destination too, so it gets planned rather
    // than reported.
    let plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(
        plan.contains("archived on success (terminal destination)"),
        "{plan}"
    );
    assert!(plan.contains("the recorded review objects"), "{plan}");
    assert!(!plan.contains("Land run"), "{plan}");

    let production = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        production.contains("landed bare-after-lane"),
        "{production}"
    );

    let bundle = read_named_bundle(&workspace, "bare-after-lane");
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    let landings = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"].as_str() == Some("feature.landed"))
        .collect::<Vec<_>>();
    assert_eq!(landings.len(), 2, "{bundle}");
    assert_eq!(landings[0]["landing"]["lane"].as_str(), Some("staging"));
    assert_eq!(landings[0]["landing"]["terminal"].as_bool(), Some(false));
    assert_eq!(landings[1]["landing"]["terminal"].as_bool(), Some(true));
    assert!(landings[1]["landing"]["lane"].is_null(), "{}", landings[1]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_bare_request_does_not_match_a_lane_plan() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "lane plan only",
        json!({ "staging": { "defaultBranch": "staging" } }),
        &["staging"],
    );

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );

    // The plan is for the staging lane; a bare apply asks for the recorded
    // review bases, which is a different destination.
    let mismatched =
        knit_fails_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(mismatched.contains("lane `staging`"), "{mismatched}");
    assert!(mismatched.contains("--lane staging"), "{mismatched}");

    // Nothing moved.
    for repo_id in ["backend", "frontend"] {
        let remote = root.join(format!("{repo_id}.git"));
        let staging = git(&remote, ["log", "--oneline", "staging"]);
        assert!(!staging.contains("Lane change"), "{staging}");
        let main = git(&remote, ["log", "--oneline", "main"]);
        assert!(!main.contains("Lane change"), "{main}");
    }
    let bundle = read_named_bundle(&workspace, "lane-plan-only");
    assert_eq!(bundle["state"].as_str(), Some("open"));
    assert!(
        !bundle["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["type"].as_str() == Some("feature.landed")),
        "{bundle}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_hand_edited_plan_that_merges_reviews_without_finishing_the_bundle_warns() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    git(&backend, ["branch", "staging", "main"]);
    git(&backend, ["push", "origin", "staging"]);
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({ "provider": "github" });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "dead end target"]);
    let feature = workspace.join(".knit/worktrees/dead-end-target/backend");
    append_line(&feature.join("app.txt"), "staging change");
    knit(&workspace, ["commit", "--all", "-m", "Staging change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    // `staging` is not backend's configured base and nothing declares it
    // terminal, so the generated plan merges the feature branch there and
    // leaves the review alone: nothing to warn about.
    let warning = "merges the review objects into a destination that does not finish the bundle";
    let fresh = knit_with_fake_gh(
        &workspace,
        ["land", "--target", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        fresh.contains("stays open on success (intermediate destination)"),
        "{fresh}"
    );
    assert!(
        fresh.contains("feature branches into the destination; review objects stay open"),
        "{fresh}"
    );
    assert!(!fresh.contains(warning), "{fresh}");

    // The plan file is editable. Turn the branch merge into a review merge by
    // hand: that is the dead end, and it is what the warning is for.
    let plan_path = workspace.join(".knit/land-plans/dead-end-target.land.json");
    let mut plan_json: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    assert_eq!(plan_json["steps"][0]["type"].as_str(), Some("merge_branch"));
    plan_json["steps"][0] = json!({
        "id": "merge-backend",
        "type": "merge_pr",
        "repoId": "backend",
        "method": "merge",
        "waitForChecks": true,
        "requiredChecksOnly": true,
        "deleteBranch": false,
        "timeoutSeconds": 1800,
        "intervalSeconds": 10
    });
    fs::write(
        &plan_path,
        format!("{}\n", serde_json::to_string_pretty(&plan_json).unwrap()),
    )
    .unwrap();

    let plan = knit_with_fake_gh(
        &workspace,
        ["land", "--target", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        plan.contains("stays open on success (intermediate destination)"),
        "{plan}"
    );
    assert!(plan.contains("the recorded review objects"), "{plan}");
    assert!(plan.contains("warning:"), "{plan}");
    assert!(plan.contains(warning), "{plan}");
    assert!(
        plan.contains("knit bundle archive dead-end-target"),
        "{plan}"
    );

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "--target", "staging", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains(warning), "{apply}");
    assert!(apply.contains("retargeted"), "{apply}");
    assert!(apply.contains("bundle stays open"), "{apply}");
    assert!(fake_gh_dir.join("merged-backend").exists());

    // A terminal plan does not carry the warning.
    let (_other_remote, other, _other_collaborator) = init_remote_repo(&root, "other");
    let other_workspace = root.join("other-workspace");
    fs::create_dir_all(&other_workspace).unwrap();
    knit(&other_workspace, ["init", "demo"]);
    knit(
        &other_workspace,
        ["project", "add", "other", other.to_str().unwrap()],
    );
    knit(&other_workspace, ["bundle", "plain landing"]);
    append_line(
        &other_workspace.join(".knit/worktrees/plain-landing/other/app.txt"),
        "plain change",
    );
    knit(&other_workspace, ["commit", "--all", "-m", "Plain change"]);
    knit_with_fake_gh(
        &other_workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    let plain = knit_with_fake_gh(&other_workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(
        plain.contains("archived on success (terminal destination)"),
        "{plain}"
    );
    assert!(!plain.contains(warning), "{plain}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn intermediate_lane_refuses_to_land_on_its_own_review_base() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "colliding lane",
        json!({ "staging": { "defaultBranch": "master" } }),
        &["master"],
    );

    // Publish the reviews against the same branch the lane merges into.
    let bundle_path = workspace.join(".knit/bundles/colliding-lane.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    for publication in bundle["publications"].as_array_mut().unwrap() {
        publication["baseBranch"] = json!("master");
    }
    fs::write(
        &bundle_path,
        format!("{}\n", serde_json::to_string_pretty(&bundle).unwrap()),
    )
    .unwrap();

    let failure = knit_fails_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        failure.contains("which is the base of its recorded review"),
        "{failure}"
    );
    assert!(failure.contains("declare the lane terminal"), "{failure}");
    assert!(!workspace
        .join(".knit/land-plans/colliding-lane.land.json")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_lane_skips_repositories_declared_absent_from_it() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "absent lane work",
        json!({
            "staging": {
                "branches": { "backend": "staging", "frontend": null }
            }
        }),
        &["staging"],
    );

    let planned = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(planned.contains("backend -> staging"), "{planned}");
    assert!(planned.contains("Not in this lane:"), "{planned}");
    assert!(
        planned.contains("merge-backend"),
        "backend should still merge: {planned}"
    );
    assert!(
        !planned.contains("merge-frontend"),
        "an absent repo gets no step: {planned}"
    );

    let plan: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/land-plans/absent-lane-work.land.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(plan["laneAbsent"], json!(["frontend"]));
    assert_eq!(plan["targetBranches"], json!({ "backend": "staging" }));
    assert_eq!(plan["terminal"], json!(false));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_lane_that_carries_nothing_this_bundle_changed_is_refused() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "all absent work",
        json!({
            "staging": {
                "branches": { "backend": null, "frontend": null }
            }
        }),
        &["staging"],
    );

    let failure = knit_fails_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        failure.contains("carries none of the repositories this bundle changed"),
        "{failure}"
    );
    assert!(failure.contains("backend"), "{failure}");
    assert!(failure.contains("frontend"), "{failure}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_unmapped_lane_repository_still_errors_and_offers_absence() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "unmapped lane work",
        json!({
            "staging": {
                "branches": { "backend": "staging" }
            }
        }),
        &["staging"],
    );

    let failure = knit_fails_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        failure.contains("has no branch for repository `frontend`"),
        "{failure}"
    );
    assert!(
        failure.contains("\"frontend\": null"),
        "the error should offer absence as a way out: {failure}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_lane_cannot_both_skip_a_repository_and_deploy_it() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "absent deploy work",
        json!({
            "staging": {
                "branches": { "backend": "staging", "frontend": null },
                "deployments": [{
                    "id": "deploy-frontend",
                    "repoId": "frontend",
                    "command": ["sh", "-c", "true"]
                }]
            }
        }),
        &["staging"],
    );

    let failure = knit_fails_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        failure.contains("declares `frontend` absent") && failure.contains("deploy-frontend"),
        "{failure}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// Artifact landing is the hosted path, and an intermediate lane must behave
/// there exactly as it does locally: merge the feature branch into the
/// environment, leave the review open against the destination that ends the
/// bundle's life.
#[test]
fn artifact_intermediate_lane_merges_the_branch_and_spares_the_review() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact staging"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let artifact = workspace.join(".knit/bundles/artifact-staging.bundle.json");
    let mut payload: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    payload["publications"] = json!([{
        "repoId": "backend",
        "provider": "github",
        "kind": "pull_request",
        "number": 101,
        "url": "https://github.com/acme/backend/pull/101",
        "baseBranch": "main",
        "headBranch": "knit/artifact-staging",
        "state": "OPEN",
        "title": "artifact staging (backend)",
        "updatedAt": "2026-06-06T00:00:00.000Z"
    }]);
    fs::write(&artifact, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let api_base = spawn_fake_github_api(&fake_gh_dir);
    let out = root.join("artifact-staging.out.bundle.json");
    let landed = knit_with_fake_gh_env(
        &root,
        vec![
            "land".into(),
            "--lane".into(),
            "staging".into(),
            "--repo-target".into(),
            "backend=staging".into(),
            "apply".into(),
            "--from-artifact".into(),
            artifact.to_string_lossy().to_string(),
            "--out".into(),
            out.to_string_lossy().to_string(),
            "--intermediate".into(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &[
            ("GH_TOKEN", "gho_fake_token"),
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
            ("KNIT_GITHUB_API_BASE", api_base.as_str()),
        ],
    );

    assert!(
        landed.contains("merged backend knit/artifact-staging -> staging"),
        "{landed}"
    );
    assert!(
        !landed.contains("retargeted"),
        "an intermediate lane must not move the review: {landed}"
    );
    // The host merged the branch, not the pull request.
    assert!(fake_gh_dir.join("api-backend-merges.json").exists());
    assert!(!fake_gh_dir.join("api-backend-merge.json").exists());
    assert!(!fake_gh_dir.join("api-backend-edit.json").exists());

    let landed_payload: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(landed_payload["publications"][0]["baseBranch"], "main");
    assert_eq!(landed_payload["publications"][0]["state"], "OPEN");
    let landing = &landed_payload["nodes"].as_array().unwrap().last().unwrap()["landing"];
    assert_eq!(landing["terminal"], json!(false));
    assert_eq!(landing["lane"], json!("staging"));

    fs::remove_dir_all(root).unwrap();
}

/// The same rule the local plan enforces, enforced on the hosted path: a lane
/// that sends a repository to the branch its review is based on would close
/// that review, so it cannot claim to be a stop along the way.
#[test]
fn artifact_intermediate_lane_refuses_to_merge_into_the_review_base() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact collide"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let artifact = workspace.join(".knit/bundles/artifact-collide.bundle.json");
    let mut payload: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    payload["publications"] = json!([{
        "repoId": "backend",
        "provider": "github",
        "kind": "pull_request",
        "number": 101,
        "url": "https://github.com/acme/backend/pull/101",
        "baseBranch": "main",
        "headBranch": "knit/artifact-collide",
        "state": "OPEN",
        "title": "artifact collide (backend)",
        "updatedAt": "2026-06-06T00:00:00.000Z"
    }]);
    fs::write(&artifact, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let api_base = spawn_fake_github_api(&fake_gh_dir);
    let out = root.join("artifact-collide.out.bundle.json");
    let failure = knit_fails_with_fake_gh_env(
        &root,
        vec![
            "land".into(),
            "--lane".into(),
            "staging".into(),
            "--repo-target".into(),
            "backend=main".into(),
            "apply".into(),
            "--from-artifact".into(),
            artifact.to_string_lossy().to_string(),
            "--out".into(),
            out.to_string_lossy().to_string(),
            "--intermediate".into(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &[
            ("GH_TOKEN", "gho_fake_token"),
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
            ("KNIT_GITHUB_API_BASE", api_base.as_str()),
        ],
    );

    assert!(
        failure.contains("which is the base of its recorded review"),
        "{failure}"
    );
    assert!(!fake_gh_dir.join("api-backend-merges.json").exists());

    fs::remove_dir_all(root).unwrap();
}

/// A repository the lane does not carry is skipped on the hosted path too,
/// and the landing still refuses when nothing is left to carry.
#[test]
fn artifact_lane_accepts_absent_repositories() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact absent"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let artifact = workspace.join(".knit/bundles/artifact-absent.bundle.json");
    let mut payload: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    payload["publications"] = json!([{
        "repoId": "backend",
        "provider": "github",
        "kind": "pull_request",
        "number": 101,
        "url": "https://github.com/acme/backend/pull/101",
        "baseBranch": "main",
        "headBranch": "knit/artifact-absent",
        "state": "OPEN",
        "title": "artifact absent (backend)",
        "updatedAt": "2026-06-06T00:00:00.000Z"
    }]);
    fs::write(&artifact, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let api_base = spawn_fake_github_api(&fake_gh_dir);
    let out = root.join("artifact-absent.out.bundle.json");
    let env = [
        ("GH_TOKEN", "gho_fake_token"),
        ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
        ("KNIT_GITHUB_API_BASE", api_base.as_str()),
    ];
    let failure = knit_fails_with_fake_gh_env(
        &root,
        vec![
            "land".into(),
            "--lane".into(),
            "staging".into(),
            "--repo-absent".into(),
            "backend".into(),
            "apply".into(),
            "--from-artifact".into(),
            artifact.to_string_lossy().to_string(),
            "--out".into(),
            out.to_string_lossy().to_string(),
            "--intermediate".into(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &env,
    );
    assert!(
        failure.contains("carries none of this bundle's published repositories"),
        "{failure}"
    );

    // Naming a repository both ways is a contradiction, not a preference.
    let contradiction = knit_fails_with_fake_gh_env(
        &root,
        vec![
            "land".into(),
            "--lane".into(),
            "staging".into(),
            "--repo-target".into(),
            "backend=staging".into(),
            "--repo-absent".into(),
            "backend".into(),
            "apply".into(),
            "--from-artifact".into(),
            artifact.to_string_lossy().to_string(),
            "--out".into(),
            out.to_string_lossy().to_string(),
            "--intermediate".into(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &env,
    );
    assert!(
        contradiction.contains("both a lane branch and declared absent"),
        "{contradiction}"
    );

    // The hosted path enforces the same rule as a local plan: a last stop has
    // to carry everything.
    let terminal_with_absence = knit_fails_with_fake_gh_env(
        &root,
        vec![
            "land".into(),
            "--lane".into(),
            "staging".into(),
            "--repo-absent".into(),
            "backend".into(),
            "apply".into(),
            "--from-artifact".into(),
            artifact.to_string_lossy().to_string(),
            "--out".into(),
            out.to_string_lossy().to_string(),
            "--terminal".into(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &env,
    );
    assert!(
        terminal_with_absence.contains("declared terminal but skips backend"),
        "{terminal_with_absence}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// The hosted path enforces the local plan's rule: a terminal landing archives
/// the bundle, so every changed repository needs a review to merge, or its
/// work is stranded on a branch nobody will land.
#[test]
fn artifact_terminal_landing_refuses_to_strand_an_unpublished_repository() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _c1) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _c2) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact strand"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    for repo_id in ["backend", "frontend"] {
        append_line(
            &workspace
                .join(".knit/worktrees/artifact-strand")
                .join(repo_id)
                .join("app.txt"),
            "artifact strand change",
        );
    }
    knit(
        &workspace,
        ["commit", "--all", "-m", "Artifact strand change"],
    );

    let artifact = workspace.join(".knit/bundles/artifact-strand.bundle.json");
    let mut payload: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    for repo in payload["repos"].as_array_mut().unwrap() {
        let id = repo["id"].as_str().unwrap().to_string();
        repo["remote"] = json!(format!("https://github.com/acme/{id}.git"));
    }
    // Both repositories have recorded work; only backend has a review.
    payload["publications"] = json!([{
        "repoId": "backend",
        "provider": "github",
        "kind": "pull_request",
        "number": 101,
        "url": "https://github.com/acme/backend/pull/101",
        "baseBranch": "main",
        "headBranch": "knit/artifact-strand",
        "state": "OPEN",
        "title": "artifact strand (backend)",
        "updatedAt": "2026-06-06T00:00:00.000Z"
    }]);
    fs::write(&artifact, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let api_base = spawn_fake_github_api(&fake_gh_dir);
    let out = root.join("artifact-strand.out.bundle.json");
    let env = [
        ("GH_TOKEN", "gho_fake_token"),
        ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
        ("KNIT_GITHUB_API_BASE", api_base.as_str()),
    ];
    let args = |lifecycle: Option<&str>| {
        let mut args = vec![
            "land".to_string(),
            "apply".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
        ];
        args.extend(lifecycle.map(str::to_string));
        args
    };

    let declared = knit_fails_with_fake_gh_env(
        &root,
        args(Some("--terminal")),
        &fake_bin,
        &fake_gh_dir,
        &env,
    );
    assert!(
        declared.contains("frontend has no recorded review"),
        "{declared}"
    );
    assert!(declared.contains("archive the bundle"), "{declared}");
    assert!(
        declared.contains("land into an intermediate destination"),
        "{declared}"
    );
    // Refused before anything moved: no merge, no retarget, no output.
    assert!(!fake_gh_dir.join("api-backend-merge.json").exists());
    assert!(!fake_gh_dir.join("api-backend-edit.json").exists());
    assert!(!fake_gh_dir.join("api-backend-merges.json").exists());
    assert!(!out.exists());

    // Without a declared answer, the review on the configured base makes the
    // landing terminal, and the same rule applies.
    let inferred = knit_fails_with_fake_gh_env(&root, args(None), &fake_bin, &fake_gh_dir, &env);
    assert!(
        inferred.contains("frontend has no recorded review"),
        "{inferred}"
    );
    assert!(!fake_gh_dir.join("api-backend-merge.json").exists());
    assert!(!fake_gh_dir.join("api-backend-edit.json").exists());
    assert!(!out.exists());

    fs::remove_dir_all(root).unwrap();
}

/// A lane is an environment, reached by merging branches, so it does not need
/// a review. Without one, nothing about the landing may read as terminal:
/// an empty set of reviewed repositories is not "every repository reaches its
/// base", and the changed repositories still have to move.
#[test]
fn artifact_lane_without_reviews_is_intermediate_and_merges_branches() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact unreviewed"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/artifact-unreviewed/backend/app.txt"),
        "unreviewed",
    );
    knit(&workspace, ["commit", "--all", "-m", "Unreviewed change"]);
    let artifact = workspace.join(".knit/bundles/artifact-unreviewed.bundle.json");
    let mut payload: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    assert!(
        payload["publications"]
            .as_array()
            .is_none_or(|publications| publications.is_empty()),
        "this bundle must not carry a review: {payload}"
    );
    fs::write(&artifact, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let api_base = spawn_fake_github_api(&fake_gh_dir);
    let out = root.join("artifact-unreviewed.out.bundle.json");
    let landed = knit_with_fake_gh_env(
        &root,
        vec![
            "land".into(),
            "--lane".into(),
            "staging".into(),
            "--repo-target".into(),
            "backend=staging".into(),
            "apply".into(),
            "--from-artifact".into(),
            artifact.to_string_lossy().to_string(),
            "--out".into(),
            out.to_string_lossy().to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &[
            ("GH_TOKEN", "gho_fake_token"),
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
            ("KNIT_GITHUB_API_BASE", api_base.as_str()),
        ],
    );

    assert!(
        landed.contains("merged backend knit/artifact-unreviewed -> staging")
            || landed.contains("already there backend"),
        "the lane must merge the feature branch: {landed}"
    );
    assert!(fake_gh_dir.join("api-backend-merges.json").exists());
    assert!(!fake_gh_dir.join("api-backend-merge.json").exists());

    let landed_payload: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    let landing = &latest_node_of_type(&landed_payload, "feature.landed")["landing"];
    assert_eq!(landing["terminal"], json!(false));
    assert_eq!(landing["lane"], json!("staging"));

    fs::remove_dir_all(root).unwrap();
}

/// A destination that does not carry all of a bundle's work cannot be where
/// that work ends: archiving there would leave the skipped repositories'
/// reviews open forever.
#[test]
fn a_lane_that_skips_a_repository_is_never_terminal() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "absent terminal work",
        json!({
            // Every carried repo lands on its own configured base, which is
            // the shape that otherwise infers "terminal".
            "release": {
                "branches": { "backend": "main", "frontend": null }
            }
        }),
        &[],
    );

    // The reviews sit on a release branch, so `main` is each repo's configured
    // base but not its review base: the shape that otherwise reads "terminal".
    let bundle_path = workspace.join(".knit/bundles/absent-terminal-work.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    for publication in bundle["publications"].as_array_mut().unwrap() {
        publication["baseBranch"] = json!("release");
    }
    fs::write(&bundle_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();

    let planned = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "release"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        planned.contains("stays open on success (intermediate destination)"),
        "{planned}"
    );

    let plan: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/land-plans/absent-terminal-work.land.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(plan["terminal"], json!(false));

    fs::remove_dir_all(root).unwrap();
}

/// The way out an error offers has to be a way out. A lane that skips
/// repositories cannot be declared terminal, so the collision error must not
/// send the reader there.
#[test]
fn the_review_base_collision_error_does_not_suggest_an_impossible_fix() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "absent collide work",
        json!({
            "release": {
                "branches": { "backend": "main", "frontend": null }
            }
        }),
        &[],
    );

    let failure = knit_fails_with_fake_gh(
        &workspace,
        ["land", "--lane", "release"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        failure.contains("which is the base of its recorded review"),
        "{failure}"
    );
    assert!(
        failure.contains("cannot be terminal instead, because it skips frontend"),
        "{failure}"
    );
    assert!(
        !failure.contains("declare the lane terminal so Knit merges"),
        "the impossible fix must not be offered: {failure}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_terminal_lane_cannot_declare_a_repository_absent() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "terminal absent work",
        json!({
            "release": {
                "terminal": true,
                "branches": { "backend": "main", "frontend": null }
            }
        }),
        &[],
    );

    let failure = knit_fails_with_fake_gh(
        &workspace,
        ["land", "--lane", "release"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        failure.contains("declared terminal but skips frontend"),
        "{failure}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// Two repositories, a landing template, and commits in only the repos named.
/// Returns the workspace plus the fake-gh pair, so a caller can plan or apply.
fn publish_scoped_deployment_bundle(
    root: &Path,
    bundle_title: &str,
    landing: Value,
    changed: &[&str],
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(root, "frontend");
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

    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = landing;
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    // Both repositories are tracked; only some of them are touched. That gap
    // is the whole point: bundle membership is not evidence of change.
    knit(&workspace, ["bundle", bundle_title]);
    let slug = bundle_title.replace(' ', "-");
    for repo_id in changed {
        append_line(
            &workspace
                .join(".knit/worktrees")
                .join(&slug)
                .join(repo_id)
                .join("app.txt"),
            "scoped change",
        );
    }
    knit(&workspace, ["commit", "--all", "-m", "Scoped change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    (workspace, fake_bin, fake_gh_dir)
}

fn plan_step_ids(plan: &Value) -> Vec<String> {
    plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["id"].as_str().unwrap().to_string())
        .collect()
}

fn read_land_plan(workspace: &Path, slug: &str) -> Value {
    let path = workspace.join(format!(".knit/land-plans/{slug}.land.json"));
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn a_deployment_is_not_planned_for_a_repository_the_bundle_did_not_change() {
    let root = unique_temp_dir();
    let landing = json!({
        "provider": "github",
        "deployments": [
            { "id": "deploy-backend", "repoId": "backend", "mode": "push" },
            { "id": "deploy-frontend", "repoId": "frontend", "mode": "push" }
        ]
    });
    let (workspace, fake_bin, fake_gh_dir) =
        publish_scoped_deployment_bundle(&root, "backend only", landing, &["backend"]);

    let output = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let plan = read_land_plan(&workspace, "backend-only");
    let ids = plan_step_ids(&plan);

    assert!(ids.contains(&"deploy-backend".to_string()));
    assert!(
        !ids.contains(&"deploy-frontend".to_string()),
        "frontend is in the bundle but unchanged, so it must not be deployed: {ids:?}"
    );
    assert_eq!(
        plan["deploymentsSkipped"]["deploy-frontend"],
        json!(["frontend"])
    );
    // Recorded and printed, so a step left out on purpose is legible.
    assert!(output.contains("Deployments not run:"), "{output}");
    assert!(output.contains("deploy-frontend"), "{output}");
}

#[test]
fn a_deployment_runs_for_a_change_in_another_repository_it_watches() {
    let root = unique_temp_dir();
    // The frontend image builds the backend's client into itself, so a
    // backend-only change still has to redeploy it.
    let landing = json!({
        "provider": "github",
        "deployments": [
            {
                "id": "deploy-frontend",
                "repoId": "frontend",
                "whenChanged": ["frontend", "backend"],
                "timeoutSeconds": 60,
                "command": ["sh", "-c", "true"]
            }
        ]
    });
    let (workspace, fake_bin, fake_gh_dir) =
        publish_scoped_deployment_bundle(&root, "watched fanout", landing, &["backend"]);

    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let plan = read_land_plan(&workspace, "watched-fanout");

    assert!(plan_step_ids(&plan).contains(&"deploy-frontend".to_string()));
    assert!(plan["deploymentsSkipped"]
        .as_object()
        .is_none_or(|skipped| skipped.is_empty()));
}

#[test]
fn a_star_deployment_runs_on_every_landing() {
    let root = unique_temp_dir();
    let landing = json!({
        "provider": "github",
        "deployments": [
            {
                "id": "notify",
                "repoId": "frontend",
                "whenChanged": ["*"],
                "timeoutSeconds": 60,
                "command": ["sh", "-c", "true"]
            }
        ]
    });
    let (workspace, fake_bin, fake_gh_dir) =
        publish_scoped_deployment_bundle(&root, "always notify", landing, &["backend"]);

    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let plan = read_land_plan(&workspace, "always-notify");

    assert!(plan_step_ids(&plan).contains(&"notify".to_string()));
}

#[test]
fn a_deployment_watching_an_unknown_repository_is_refused() {
    let root = unique_temp_dir();
    let landing = json!({
        "provider": "github",
        "deployments": [
            {
                "id": "deploy-backend",
                "repoId": "backend",
                "whenChanged": ["backend", "bakcend"],
                "mode": "push"
            }
        ]
    });
    let (workspace, fake_bin, fake_gh_dir) =
        publish_scoped_deployment_bundle(&root, "typo watch", landing, &["backend"]);

    let output = knit_fails_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);

    assert!(output.contains("deploy-backend"), "{output}");
    assert!(output.contains("bakcend"), "{output}");
    assert!(output.contains("whenChanged"), "{output}");
}

#[test]
fn a_lane_deploys_only_the_applications_this_bundle_changed() {
    let root = unique_temp_dir();
    let lanes = json!({
        "staging": {
            "branches": { "backend": "staging", "frontend": "staging" },
            "deployments": [
                { "id": "stage-backend", "repoId": "backend", "mode": "push" },
                { "id": "stage-frontend", "repoId": "frontend", "mode": "push" }
            ]
        }
    });
    let landing = json!({ "provider": "github", "lanes": lanes });

    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    for checkout in [&backend, &frontend] {
        git(checkout, ["branch", "staging", "main"]);
        git(checkout, ["push", "origin", "staging"]);
    }
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
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = landing;
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "stage backend"]);
    append_line(
        &workspace.join(".knit/worktrees/stage-backend/backend/app.txt"),
        "backend only",
    );
    knit(&workspace, ["commit", "--all", "-m", "Backend only"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let plan = read_land_plan(&workspace, "stage-backend");
    let ids = plan_step_ids(&plan);

    assert!(ids.contains(&"stage-backend".to_string()));
    assert!(
        !ids.contains(&"stage-frontend".to_string()),
        "an unchanged application must not be redeployed into staging: {ids:?}"
    );
    assert_eq!(
        plan["deploymentsSkipped"]["stage-frontend"],
        json!(["frontend"])
    );
}

#[test]
fn a_bundle_with_no_recorded_work_still_runs_its_deployments() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    let marker = root.join("deploy-only-ran");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "deployments": [{
            "id": "deploy-backend",
            "repoId": "backend",
            "timeoutSeconds": 60,
            "command": ["sh", "-c", format!("touch '{}'", marker.display())]
        }]
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    // No commits, no reviews: a deploy-only plan. Scoping has no change set to
    // narrow against here, so it must not narrow the plan down to nothing.
    knit(&workspace, ["bundle", "deploy only"]);
    knit(&workspace, ["land", "plan"]);
    let plan = read_land_plan(&workspace, "deploy-only");

    assert!(plan_step_ids(&plan).contains(&"deploy-backend".to_string()));

    knit(&workspace, ["land", "apply"]);
    assert!(
        marker.exists(),
        "deploy-only plan did not run its deployment"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_terminal_landing_refuses_to_strand_a_repository_with_no_review() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _c1) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _c2) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    for (id, path) in [("backend", &backend), ("frontend", &frontend)] {
        knit(&workspace, ["project", "add", id, path.to_str().unwrap()]);
    }
    knit(&workspace, ["bundle", "both changed"]);
    for repo_id in ["backend", "frontend"] {
        append_line(
            &workspace
                .join(".knit/worktrees/both-changed")
                .join(repo_id)
                .join("app.txt"),
            "changed",
        );
    }
    knit(&workspace, ["commit", "--all", "-m", "Both changed"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    // Only one of the two changed repositories gets a review.
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync", "backend"],
        &fake_bin,
        &fake_gh_dir,
    );

    // Landing now would merge backend, archive the bundle, and remove the
    // worktrees — stranding the frontend commit on a branch nobody will land.
    let failed = knit_fails_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(failed.contains("archives the bundle"), "{failed}");
    assert!(failed.contains("frontend"), "{failed}");
    assert!(failed.contains("knit publish create"), "{failed}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_terminal_landing_refuses_a_repository_its_merge_order_excludes() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _c1) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _c2) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    for (id, path) in [("backend", &backend), ("frontend", &frontend)] {
        knit(&workspace, ["project", "add", id, path.to_str().unwrap()]);
    }
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "merge": { "repoOrder": ["backend"], "includeUnlisted": false }
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "excluded repo"]);
    for repo_id in ["backend", "frontend"] {
        append_line(
            &workspace
                .join(".knit/worktrees/excluded-repo")
                .join(repo_id)
                .join("app.txt"),
            "changed",
        );
    }
    knit(&workspace, ["commit", "--all", "-m", "Both changed"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let failed = knit_fails_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(failed.contains("frontend"), "{failed}");
    assert!(failed.contains("includeUnlisted"), "{failed}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn committing_after_planning_refuses_the_stale_plan() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    append_line(&feature.join("app.txt"), "first");
    knit(&workspace, ["commit", "--all", "-m", "First"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);

    // The plan describes the bundle as it was. More work makes it a
    // description of the past.
    append_line(&feature.join("app.txt"), "second");
    knit(&workspace, ["commit", "--all", "-m", "Second"]);

    let failed = knit_fails_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(failed.contains("different head"), "{failed}");
    assert!(failed.contains("knit land plan --force"), "{failed}");

    fs::remove_dir_all(root).unwrap();
}

/// The ledger only moves when Knit records a commit. A plain `git commit` in
/// the worktree leaves it untouched, so the plan's pins still match and the
/// landing would apply against work the plan never saw.
#[test]
fn a_plain_git_commit_after_planning_refuses_the_stale_plan() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    append_line(&feature.join("app.txt"), "first");
    knit(&workspace, ["commit", "--all", "-m", "First"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);

    // Committed with git directly: the ledger does not know about it.
    append_line(&feature.join("app.txt"), "second");
    git(&feature, ["add", "app.txt"]);
    git(&feature, ["commit", "-m", "Second"]);

    let failed = knit_fails_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(
        failed.contains("backend have commits in their worktrees that Knit has not recorded"),
        "{failed}"
    );
    assert!(failed.contains("knit sync"), "{failed}");
    assert!(failed.contains("knit land plan --force"), "{failed}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_update_repins_the_plan_it_prepared() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "feature",
    );
    knit(&workspace, ["commit", "--all", "-m", "Feature"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);

    fs::write(backend_collaborator.join("base.txt"), "base moved\n").unwrap();
    git(&backend_collaborator, ["add", "base.txt"]);
    git(&backend_collaborator, ["commit", "-m", "Base moved"]);
    git(&backend_collaborator, ["push", "origin", "main"]);

    // `knit land update` moves the feature head on purpose. The documented
    // recovery is update-then-land, so the plan's own state check must not
    // turn that into a dead end.
    knit_with_fake_gh(&workspace, ["land", "update"], &fake_bin, &fake_gh_dir);
    let applied = knit_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(applied.contains("Feature landed"), "{applied}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_skipped_deployment_a_running_one_needs_is_reinstated() {
    let root = unique_temp_dir();
    let landing = json!({
        "provider": "github",
        "deployments": [
            {
                "id": "migrate-frontend",
                "repoId": "frontend",
                "timeoutSeconds": 60,
                "command": ["sh", "-c", "true"]
            },
            {
                "id": "deploy-backend",
                "repoId": "backend",
                "needs": ["migrate-frontend"],
                "timeoutSeconds": 60,
                "command": ["sh", "-c", "true"]
            }
        ]
    });
    let (workspace, fake_bin, fake_gh_dir) =
        publish_scoped_deployment_bundle(&root, "needs skipped", landing, &["backend"]);

    // frontend is unchanged, so its deployment would be skipped — but the
    // backend deployment that does run declares it as a dependency. Dropping
    // it used to make the whole plan refuse with "needs unknown step".
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let plan = read_land_plan(&workspace, "needs-skipped");
    let ids = plan_step_ids(&plan);

    assert!(ids.contains(&"deploy-backend".to_string()), "{ids:?}");
    assert!(ids.contains(&"migrate-frontend".to_string()), "{ids:?}");
    assert!(plan["deploymentsSkipped"]
        .as_object()
        .is_none_or(|skipped| skipped.is_empty()));
}

#[test]
fn a_cross_repo_trigger_still_receives_its_lane_target_branch() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _c1) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _c2) = init_remote_repo(&root, "frontend");
    for checkout in [&backend, &frontend] {
        git(checkout, ["branch", "staging", "main"]);
        git(checkout, ["push", "origin", "staging"]);
    }
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "demo"]);
    for (id, path) in [("backend", &backend), ("frontend", &frontend)] {
        knit(&workspace, ["project", "add", id, path.to_str().unwrap()]);
    }
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "lanes": {
            "staging": {
                "branches": { "backend": "staging", "frontend": "staging" },
                "deployments": [{
                    "id": "stage-frontend",
                    "repoId": "frontend",
                    "whenChanged": ["frontend", "backend"],
                    "timeoutSeconds": 60,
                    "command": ["sh", "-c", "true"]
                }]
            }
        }
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "cross lane"]);
    append_line(
        &workspace.join(".knit/worktrees/cross-lane/backend/app.txt"),
        "backend only",
    );
    knit(&workspace, ["commit", "--all", "-m", "Backend only"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );

    let plan = read_land_plan(&workspace, "cross-lane");
    let step = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["id"].as_str() == Some("stage-frontend"))
        .expect("the triggered deployment is planned");
    // frontend has no merge step in this landing, so its branch has to come
    // from the lane rather than from this plan's merge projection.
    assert_eq!(
        step["env"]["KNIT_LAND_TARGET_BRANCH"].as_str(),
        Some("staging")
    );
    assert_eq!(step["env"]["KNIT_LAND_LANE"].as_str(), Some("staging"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_cross_repo_push_deployment_is_refused() {
    let root = unique_temp_dir();
    let landing = json!({
        "provider": "github",
        "deployments": [{
            "id": "deploy-frontend",
            "repoId": "frontend",
            "whenChanged": ["frontend", "backend"],
            "mode": "push"
        }]
    });
    let (workspace, fake_bin, fake_gh_dir) =
        publish_scoped_deployment_bundle(&root, "cross push", landing, &["backend"]);

    // A push deployment only reports that its own repository's merge triggered
    // it; watching another repository would make that report false.
    let failed = knit_fails_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(failed.contains("deploy-frontend"), "{failed}");
    assert!(failed.contains("push mode"), "{failed}");
}

#[test]
fn when_changed_rejects_wildcards_mixed_with_names_empties_and_repeats() {
    for (case, when_changed, expected) in [
        ("typo beside a wildcard", json!(["*", "bakcend"]), "*"),
        ("empty list", json!([]), "empty"),
        ("repeated entry", json!(["backend", "backend"]), "repeats"),
    ] {
        let root = unique_temp_dir();
        let landing = json!({
            "provider": "github",
            "deployments": [{
                "id": "deploy-backend",
                "repoId": "backend",
                "whenChanged": when_changed,
                "timeoutSeconds": 60,
                "command": ["sh", "-c", "true"]
            }]
        });
        let (workspace, fake_bin, fake_gh_dir) =
            publish_scoped_deployment_bundle(&root, "bad watch", landing, &["backend"]);

        let failed = knit_fails_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
        assert!(
            failed.contains("deploy-backend") && failed.contains(expected),
            "{case}: {failed}"
        );
    }
}

#[test]
fn a_cross_repo_trigger_survives_branch_target_selection() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _c1) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _c2) = init_remote_repo(&root, "frontend");
    for checkout in [&backend, &frontend] {
        git(checkout, ["branch", "release", "main"]);
        git(checkout, ["push", "origin", "release"]);
    }
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "demo"]);
    for (id, path) in [("backend", &backend), ("frontend", &frontend)] {
        knit(&workspace, ["project", "add", id, path.to_str().unwrap()]);
    }
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "targets": {
            "release": {
                "deployments": [{
                    "id": "release-frontend",
                    "repoId": "frontend",
                    "whenChanged": ["frontend", "backend"],
                    "timeoutSeconds": 60,
                    "command": ["sh", "-c", "true"]
                }]
            }
        }
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "target trigger"]);
    append_line(
        &workspace.join(".knit/worktrees/target-trigger/backend/app.txt"),
        "backend only",
    );
    knit(&workspace, ["commit", "--all", "-m", "Backend only"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        [
            "publish",
            "create",
            "--github",
            "--no-sync",
            "--base",
            "release",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);

    let plan = read_land_plan(&workspace, "target-trigger");
    // "does this repository land into this branch?" is not a question a
    // deployment triggered by *another* repository can answer, and answering
    // it first used to discard the step before the trigger was considered.
    assert!(
        plan_step_ids(&plan).contains(&"release-frontend".to_string()),
        "{:?}",
        plan_step_ids(&plan)
    );

    fs::remove_dir_all(root).unwrap();
}

/// `--target <branch>` is an ad-hoc lane. When the branch is not the bundle's
/// terminal destination, landing there is a stop along the way: the feature
/// branches are merged into it, the reviews stay open against the terminal
/// destination, and the bundle goes on to land there afterwards.
#[test]
fn an_intermediate_target_merges_branches_and_keeps_the_bundle_open() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    git(&backend, ["branch", "staging", "main"]);
    git(&backend, ["push", "origin", "staging"]);
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({ "provider": "github" });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "staging pass"]);
    let feature = workspace.join(".knit/worktrees/staging-pass/backend");
    append_line(&feature.join("app.txt"), "staging change");
    knit(&workspace, ["commit", "--all", "-m", "Staging change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let warning = "merges the review objects into a destination that does not finish the bundle";
    let plan = knit_with_fake_gh(
        &workspace,
        ["land", "--target", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        plan.contains("stays open on success (intermediate destination)"),
        "{plan}"
    );
    assert!(
        plan.contains("feature branches into the destination; review objects stay open"),
        "{plan}"
    );
    assert!(plan.contains("backend -> staging"), "{plan}");
    assert!(!plan.contains(warning), "{plan}");
    let plan_json = read_land_plan(&workspace, "staging-pass");
    assert_eq!(plan_json["targetBranch"].as_str(), Some("staging"));
    assert_eq!(plan_json["terminal"].as_bool(), Some(false));
    assert!(plan_json["targetBranches"].is_null() || plan_json["targetBranches"] == json!({}));
    assert_eq!(plan_json["steps"][0]["type"].as_str(), Some("merge_branch"));
    assert_eq!(
        plan_json["steps"][0]["targetBranch"].as_str(),
        Some("staging")
    );

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "--target", "staging", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("bundle stays open"), "{apply}");
    assert!(!apply.contains(warning), "{apply}");
    assert!(!apply.contains("retargeted"), "{apply}");

    // The environment got the work by branch merge; the review was neither
    // retargeted nor merged.
    assert!(!fake_gh_dir.join("retarget-order.txt").exists());
    assert!(!fake_gh_dir.join("merge-order.txt").exists());
    assert!(!fake_gh_dir.join("merged-backend").exists());
    let staging = git(&backend_remote, ["log", "--oneline", "staging"]);
    assert!(staging.contains("Staging change"), "{staging}");
    let main = git(&backend_remote, ["log", "--oneline", "main"]);
    assert!(!main.contains("Staging change"), "{main}");

    let bundle = read_named_bundle(&workspace, "staging-pass");
    assert_eq!(bundle["state"].as_str(), Some("open"));
    assert_eq!(
        bundle["publications"][0]["baseBranch"].as_str(),
        Some("main")
    );
    assert_eq!(bundle["publications"][0]["state"].as_str(), Some("OPEN"));
    let landed = latest_node_of_type(&bundle, "feature.landed");
    assert_eq!(landed["landing"]["terminal"].as_bool(), Some(false));
    assert_eq!(landed["landing"]["targetBranch"].as_str(), Some("staging"));
    assert!(landed["landing"]["lane"].is_null(), "{landed}");
    assert!(workspace
        .join(".knit/worktrees/staging-pass/backend")
        .exists());

    // The bundle then lands at its terminal destination: a bare request plans
    // the recorded review bases and merges the review.
    let next = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(
        next.contains("archived on success (terminal destination)"),
        "{next}"
    );
    assert!(next.contains("the recorded review objects"), "{next}");
    let finish = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(finish.contains("landed staging-pass"), "{finish}");
    assert!(fake_gh_dir.join("merged-backend").exists());
    let bundle = read_named_bundle(&workspace, "staging-pass");
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    let landings = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"].as_str() == Some("feature.landed"))
        .collect::<Vec<_>>();
    assert_eq!(landings.len(), 2, "{bundle}");
    assert_eq!(landings[1]["landing"]["terminal"].as_bool(), Some(true));

    fs::remove_dir_all(root).unwrap();
}

/// The other half of the rule: a `--target` that is the bundle's terminal
/// destination merges the recorded reviews and archives the bundle.
#[test]
fn a_terminal_target_still_merges_the_reviews() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({ "provider": "github" });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "main target"]);
    let feature = workspace.join(".knit/worktrees/main-target/backend");
    append_line(&feature.join("app.txt"), "main change");
    knit(&workspace, ["commit", "--all", "-m", "Main change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let plan = knit_with_fake_gh(
        &workspace,
        ["land", "--target", "main"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        plan.contains("archived on success (terminal destination)"),
        "{plan}"
    );
    assert!(plan.contains("the recorded review objects"), "{plan}");
    let plan_json = read_land_plan(&workspace, "main-target");
    assert_eq!(plan_json["targetBranch"].as_str(), Some("main"));
    assert_eq!(plan_json["terminal"].as_bool(), Some(true));
    assert_eq!(plan_json["steps"][0]["type"].as_str(), Some("merge_pr"));

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "--target", "main", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("landed main-target"), "{apply}");
    // The review already pointed at main, so it was merged without a retarget.
    assert!(!fake_gh_dir.join("retarget-order.txt").exists());
    assert!(fake_gh_dir.join("merged-backend").exists());
    // The fake forge merges nothing in git; the branch itself was not merged
    // by Knit into main.
    let main = git(&backend_remote, ["log", "--oneline", "main"]);
    assert!(!main.contains("Main change"), "{main}");

    let bundle = read_named_bundle(&workspace, "main-target");
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    let landed = latest_node_of_type(&bundle, "feature.landed");
    assert_eq!(landed["landing"]["terminal"].as_bool(), Some(true));
    assert_eq!(landed["landing"]["targetBranch"].as_str(), Some("main"));

    fs::remove_dir_all(root).unwrap();
}

/// The artifact path follows the same rule: an intermediate `--target` merges
/// the feature branch on the host and leaves the review untouched.
#[test]
fn artifact_intermediate_target_merges_the_branch_and_spares_the_review() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact target"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let artifact = workspace.join(".knit/bundles/artifact-target.bundle.json");
    let mut payload: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    payload["publications"] = json!([{
        "repoId": "backend",
        "provider": "github",
        "kind": "pull_request",
        "number": 101,
        "url": "https://github.com/acme/backend/pull/101",
        "baseBranch": "main",
        "headBranch": "knit/artifact-target",
        "state": "OPEN",
        "title": "artifact target (backend)",
        "updatedAt": "2026-06-06T00:00:00.000Z"
    }]);
    fs::write(&artifact, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let api_base = spawn_fake_github_api(&fake_gh_dir);
    let out = root.join("artifact-target.out.bundle.json");
    let landed = knit_with_fake_gh_env(
        &root,
        vec![
            "land".into(),
            "--target".into(),
            "staging".into(),
            "apply".into(),
            "--from-artifact".into(),
            artifact.to_string_lossy().to_string(),
            "--out".into(),
            out.to_string_lossy().to_string(),
            "--intermediate".into(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &[
            ("GH_TOKEN", "gho_fake_token"),
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
            ("KNIT_GITHUB_API_BASE", api_base.as_str()),
        ],
    );

    assert!(
        landed.contains("merged backend knit/artifact-target -> staging"),
        "{landed}"
    );
    assert!(
        !landed.contains("retargeted"),
        "an intermediate target must not move the review: {landed}"
    );
    // The host merged the branch, not the pull request.
    assert!(fake_gh_dir.join("api-backend-merges.json").exists());
    assert!(!fake_gh_dir.join("api-backend-merge.json").exists());
    assert!(!fake_gh_dir.join("api-backend-edit.json").exists());

    let landed_payload: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(landed_payload["publications"][0]["baseBranch"], "main");
    assert_eq!(landed_payload["publications"][0]["state"], "OPEN");
    let landing = &landed_payload["nodes"].as_array().unwrap().last().unwrap()["landing"];
    assert_eq!(landing["terminal"], json!(false));
    assert_eq!(landing["targetBranch"], json!("staging"));
    assert!(landing["lane"].is_null(), "{landing}");

    fs::remove_dir_all(root).unwrap();
}

/// The lane merge runs in a detached managed checkout and pushes its HEAD to
/// the lane branch. A source clone that happens to sit on that branch -- as a
/// team's clones do on their staging branch -- is never entered: it keeps its
/// commit and its uncommitted edits. Knit used to fall back to merging inside
/// that clone when `git worktree add <branch>` was refused.
#[test]
fn a_lane_landing_never_touches_a_source_checkout_on_the_lane_branch() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_lane_bundle(
        &root,
        "staging work",
        json!({ "staging": { "defaultBranch": "staging" } }),
        &["staging"],
    );

    let backend = root.join("backend");
    git(&backend, ["checkout", "staging"]);
    let source_sha_before = git(&backend, ["rev-parse", "HEAD"]);
    append_line(&backend.join("app.txt"), "uncommitted local edit");
    let source_app_before = fs::read_to_string(backend.join("app.txt")).unwrap();
    assert!(git(&backend, ["status", "--porcelain"]).contains(" M app.txt"));

    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("bundle stays open"), "{apply}");
    assert!(apply.contains("left alone"), "{apply}");

    // Origin got the merge.
    let backend_remote = root.join("backend.git");
    let staging_log = git(&backend_remote, ["log", "--oneline", "staging"]);
    assert!(staging_log.contains("Lane change"), "{staging_log}");

    // The source clone was not entered: same branch, same commit, same
    // uncommitted edit.
    assert_eq!(
        git(&backend, ["branch", "--show-current"]).trim(),
        "staging"
    );
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), source_sha_before);
    assert_eq!(
        fs::read_to_string(backend.join("app.txt")).unwrap(),
        source_app_before
    );
    assert!(git(&backend, ["status", "--porcelain"]).contains(" M app.txt"));

    // The merge was made in a detached managed checkout.
    let managed = workspace.join(".knit/merge-worktrees/staging/backend");
    assert!(managed.exists(), "{}", managed.display());
    assert!(!git_success(&managed, ["symbolic-ref", "-q", "HEAD"]));
    assert_eq!(
        git(&managed, ["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "HEAD"
    );
    assert_eq!(
        git(&managed, ["rev-parse", "HEAD"]),
        git(&backend_remote, ["rev-parse", "staging"])
    );

    // A local lane branch no checkout holds still follows the merge.
    let frontend = root.join("frontend");
    assert_eq!(git(&frontend, ["branch", "--show-current"]).trim(), "main");
    assert_eq!(
        git(&frontend, ["rev-parse", "refs/heads/staging"]),
        git(&root.join("frontend.git"), ["rev-parse", "staging"])
    );

    fs::remove_dir_all(root).unwrap();
}
