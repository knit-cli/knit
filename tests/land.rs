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
            "apply".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
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
    assert!(landed.contains("checks backend"), "{landed}");
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

    let landed_bundle: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        landed_bundle["publications"][0]["state"].as_str(),
        Some("MERGED")
    );
    assert!(landed_bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["type"].as_str() == Some("feature.landed")));

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
    assert!(sync_error.contains("No KnitHub remote configured"));

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
    let deploy_script = format!(
        "pwd > '{}' && git rev-parse --abbrev-ref HEAD > '{}' && test -f base.txt",
        deploy_pwd.display(),
        deploy_branch.display()
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
    assert_eq!(
        steps[2]["needs"].as_array().unwrap()[0].as_str(),
        Some("merge-backend")
    );
    assert_eq!(steps[3]["id"].as_str(), Some("deploy-frontend"));
    assert_eq!(
        steps[3]["needs"].as_array().unwrap()[0].as_str(),
        Some("merge-frontend")
    );

    let apply = knit_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(apply.contains("deploy-backend"));
    assert!(apply.contains("deploy-frontend"));
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
    plan["steps"].as_array_mut().unwrap().push(json!({
        "id": "deploy",
        "type": "run",
        "cwd": ".",
        "command": ["sh", "-c", "test \"$DEPLOY_OK\" = \"yes\" && test -f deploy-ok"],
        "env": { "DEPLOY_OK": "yes" },
        "needs": ["merge-backend"]
    }));
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    let failed = knit_fails_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(failed.contains("stopped at step deploy"));
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
    let status = knit_with_fake_gh(&workspace, ["land", "status"], &fake_bin, &fake_gh_dir);
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
