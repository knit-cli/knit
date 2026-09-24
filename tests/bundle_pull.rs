mod common;

use common::*;
use std::fs;
use std::path::Path;

/// Export with one local-path repo and one bundle whose feature branch exists
/// on the repo's origin, so `bundle pull` has something real to fetch,
/// branch, and materialize.
fn export_with_feature_bundle(root: &Path) -> (serde_json::Value, String) {
    let source = root.join("backend-source");
    init_repo(&source, "backend");
    git(&source, ["branch", "knit/feature-a"]);
    let head = git(&source, ["rev-parse", "main"]);

    let export = serde_json::json!({
        "data": {
            "project": {"id": "p-1", "slug": "demo"},
            "knitProject": null,
            "repositories": [{
                "id": "r-1",
                "localId": "backend",
                "name": "backend",
                "defaultBranch": "main",
                "remoteUrl": source.to_string_lossy(),
                "visibility": "public",
                "metadata": {},
            }],
            "omittedRepositoryCount": 0,
            "bundles": [{
                "id": "rb-1",
                "slug": "feature-a",
                "lifecycleState": "open",
                "currentArtifact": {
                    "artifactHash": "hash-a",
                    "payload": {
                        "schemaVersion": "1",
                        "kind": "knit.bundle",
                        "id": "feature-a",
                        "title": "feature a",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "repos": [{
                            "id": "backend",
                            "path": "/elsewhere/backend",
                            "baseBranch": "main",
                            "featureBranch": "knit/feature-a",
                        }],
                        "commitGroups": [],
                    },
                },
            }],
            "historyEvents": [],
        }
    });
    (export, head.trim().to_string())
}

fn cloned_workspace(root: &Path, base_url: &str) -> std::path::PathBuf {
    let target = root.join("workspace");
    let (_stdout, stderr, success) = knit_split_output(
        root,
        &[
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
        &[],
    );
    assert!(success, "clone failed: {stderr}");
    target
}

#[test]
fn bundle_pull_fetches_branches_and_materializes_worktrees() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake");
    let (export, head) = export_with_feature_bundle(&root);
    let base_url = spawn_fake_remote_api(&fake_dir, export.to_string());
    let workspace = cloned_workspace(&root, &base_url);

    let (stdout, stderr, success) =
        knit_split_output(&workspace, &["bundle", "pull", "feature-a", "--json"], &[]);
    assert!(success, "bundle pull failed: {stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout must be pure JSON ({error}): {stdout}"));

    assert_eq!(document["bundle"], "feature-a");
    let repos = document["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["id"], "backend");
    assert_eq!(repos[0]["featureBranch"], "knit/feature-a");
    assert_eq!(repos[0]["status"], "pulled");
    assert_eq!(repos[0]["headSha"].as_str().unwrap(), head);
    let worktree_path = repos[0]["worktreePath"].as_str().unwrap();
    assert!(
        worktree_path.ends_with(&format!(
            ".knit{0}worktrees{0}feature-a{0}backend",
            std::path::MAIN_SEPARATOR
        )) || worktree_path.ends_with(".knit/worktrees/feature-a/backend"),
        "unexpected worktree path: {worktree_path}"
    );
    assert!(Path::new(worktree_path).join(".git").exists());

    // The checkout sits on the feature branch.
    let branch = git(
        Path::new(worktree_path),
        ["rev-parse", "--abbrev-ref", "HEAD"],
    );
    assert_eq!(branch.trim(), "knit/feature-a");

    // Pulling again is idempotent: artifact already current, worktree kept.
    let (stdout, stderr, success) =
        knit_split_output(&workspace, &["bundle", "pull", "feature-a", "--json"], &[]);
    assert!(success, "second bundle pull failed: {stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(document["bundle"], "feature-a");
    assert!(stderr.contains("already current"), "stderr: {stderr}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_pull_unknown_slug_is_not_found() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake");
    let (export, _head) = export_with_feature_bundle(&root);
    let base_url = spawn_fake_remote_api(&fake_dir, export.to_string());
    let workspace = cloned_workspace(&root, &base_url);

    let (stdout, _stderr, success) = knit_split_output(
        &workspace,
        &["bundle", "pull", "no-such-bundle", "--json"],
        &[],
    );
    assert!(!success);
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(envelope["error"]["kind"], "notFound");
    assert!(envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no-such-bundle"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_pull_clones_a_repository_added_to_the_remote_bundle() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake");
    let (mut export, _) = export_with_feature_bundle(&root);
    let base_url = spawn_fake_remote_api(&fake_dir, export.to_string());
    let workspace = cloned_workspace(&root, &base_url);
    assert!(!workspace.join("frontend").exists());

    let frontend = root.join("frontend-source");
    init_repo(&frontend, "frontend");
    git(&frontend, ["branch", "knit/feature-a"]);
    export["data"]["repositories"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "id":"r-2", "localId":"frontend", "name":"frontend", "defaultBranch":"main",
            "remoteUrl":frontend.to_string_lossy(), "visibility":"public", "metadata":{}
        }));
    let bundle = &mut export["data"]["bundles"][0]["currentArtifact"]["payload"];
    bundle["repos"].as_array_mut().unwrap().push(serde_json::json!({
        "id":"frontend", "path":"/elsewhere/frontend", "baseBranch":"main", "featureBranch":"knit/feature-a"
    }));
    bundle["nodes"] = serde_json::json!([{
        "id":"frontend-added", "type":"repo.added", "createdAt":"2026-09-05T12:00:00Z", "repoIds":["frontend"]
    }]);
    bundle["headNodeId"] = serde_json::json!("frontend-added");
    export["data"]["bundles"][0]["currentArtifact"]["artifactHash"] =
        serde_json::json!("hash-with-frontend");
    fs::write(fake_dir.join("export.json"), export.to_string()).unwrap();

    let (stdout, stderr, success) =
        knit_split_output(&workspace, &["bundle", "pull", "feature-a", "--json"], &[]);
    assert!(success, "bundle pull failed: {stderr}\n{stdout}");
    let document: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(document["repos"].as_array().unwrap().len(), 2);
    assert!(workspace.join("frontend/.git").exists());
    let tree = workspace.join(".knit/worktrees/feature-a/frontend");
    assert!(tree.join(".git").exists());
    assert_eq!(
        git(&tree, ["rev-parse", "HEAD"]).trim(),
        git(&frontend, ["rev-parse", "main"]).trim()
    );
    let project: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/projects/demo.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        project["repos"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|repo| repo["id"] == "frontend")
            .count(),
        1
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_bundle_pull_imports_all_destinations_and_receipts_with_scoped_query() {
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    let root = unique_temp_dir();
    let fake_dir = root.join("fake");
    let (export, _) = export_with_feature_bundle(&root);
    let base_url = spawn_fake_remote_api(&fake_dir, export.to_string());
    let workspace = cloned_workspace(&root, &base_url);
    let mut records = vec![];
    for lane in [None, Some("preview")] {
        let mut plan = json!({"kind":"KnitLandPlan","schemaVersion":"0.2","id":"plan-synthetic","bundleId":"feature-a","steps":[{"id":"inspect","type":"run","repoId":"backend","command":["synthetic-inspect"]}]});
        if let Some(lane) = lane {
            plan["lane"] = json!(lane);
        }
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&plan).unwrap()));
        records.push(json!({"bundleSlug":"feature-a","revision":1,"hash":hash,"plan":plan}));
    }
    // Simulate an older server ignoring the query. Foreign payload is invalid
    // deliberately: filtering must happen before installing/validating it.
    records.push(
        json!({"bundleSlug":"unrelated","revision":1,"hash":"bad","plan":{"bundleId":"unrelated"}}),
    );
    fs::write(fake_dir.join("landing-artifacts.json"),json!({"data":{"plans":records,"runs":[{"bundleSlug":"feature-a","run":{"kind":"KnitLandRun","schemaVersion":"0.2","id":"run-synthetic","bundleId":"feature-a","status":"succeeded"}}]}}).to_string()).unwrap();
    let (stdout, stderr, success) =
        knit_split_output(&workspace, &["bundle", "pull", "feature-a", "--json"], &[]);
    assert!(success, "{stderr}\n{stdout}");
    serde_json::from_str::<Value>(&stdout).expect("bundle JSON remains clean");
    assert!(fs::read_to_string(fake_dir.join("landing-gets.txt"))
        .unwrap()
        .contains("?bundleSlug=feature-a"));
    let plans: Vec<_> = fs::read_dir(workspace.join(".knit/land-plans"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_file())
        .collect();
    assert_eq!(plans.len(), 2);
    assert!(workspace
        .join(".knit/land-runs/run-synthetic.run.json")
        .exists());
    assert!(!workspace
        .join(".knit/land-plans/unrelated.land.json")
        .exists());
    let local = workspace.join(".knit/land-plans/feature-a.land.json");
    let mut edit: Value = serde_json::from_str(&fs::read_to_string(&local).unwrap()).unwrap();
    edit["steps"][0]["command"] = json!(["local-edit"]);
    fs::write(&local, edit.to_string()).unwrap();
    let mut remote = records[0].clone();
    remote["plan"]["steps"][0]["command"] = json!(["remote-edit"]);
    remote["revision"] = json!(2);
    remote["hash"] = json!(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&remote["plan"]).unwrap())
    ));
    fs::write(
        fake_dir.join("landing-artifacts.json"),
        json!({"data":{"plans":[remote],"runs":[]}}).to_string(),
    )
    .unwrap();
    let failed = knit_fails(&workspace, ["bundle", "pull", "feature-a"]);
    assert!(failed.contains("Local file preserved"), "{failed}");
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(local).unwrap()).unwrap(),
        edit
    );
    assert_eq!(
        fs::read_dir(workspace.join(".knit/land-plans/conflicts"))
            .unwrap()
            .count(),
        1
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_bundle_push_sends_authored_plans_with_safe_project_recipe_sync() {
    use serde_json::{json, Value};
    let root = unique_temp_dir();
    let fake_dir = root.join("fake");
    let (export, _) = export_with_feature_bundle(&root);
    let base_url = spawn_fake_remote_api(&fake_dir, export.to_string());
    let workspace = cloned_workspace(&root, &base_url);
    fs::write(
        fake_dir.join("landing-artifacts.json"),
        json!({"data":{"plans":[],"runs":[]}}).to_string(),
    )
    .unwrap();
    knit(&workspace, ["bundle", "pull", "feature-a"]);
    let plan = json!({"kind":"KnitLandPlan","schemaVersion":"0.2","id":"plan-synthetic","bundleId":"feature-a","lane":"preview","steps":[{"id":"inspect","command":["synthetic-inspect"]}]});
    fs::create_dir_all(workspace.join(".knit/land-plans")).unwrap();
    fs::write(
        workspace.join(".knit/land-plans/authored.land.json"),
        plan.to_string(),
    )
    .unwrap();
    // The previously pulled recipe establishes the CAS base for this push.
    let (_, stderr, success) = knit_split_output(
        &workspace,
        &["--bundle", "feature-a", "push", "--remote", "hosted"],
        &[],
    );
    assert!(success, "{stderr}");
    let posted = fs::read_to_string(fake_dir.join("landing-posts.jsonl")).unwrap();
    let body: Value = serde_json::from_str(posted.lines().last().unwrap()).unwrap();
    assert_eq!(body["bundleSlug"], "feature-a");
    assert_eq!(body["plans"].as_array().unwrap().len(), 1);
    assert_eq!(body["plans"][0]["plan"], plan);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ordinary_bundle_pull_updates_web_recipes_and_preserves_divergent_local_recipes() {
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    let root = unique_temp_dir();
    let fake_dir = root.join("fake");
    let (export, _) = export_with_feature_bundle(&root);
    let base_url = spawn_fake_remote_api(&fake_dir, export.to_string());
    let workspace = cloned_workspace(&root, &base_url);
    fs::write(
        fake_dir.join("landing-artifacts.json"),
        json!({"data":{"plans":[],"runs":[]}}).to_string(),
    )
    .unwrap();
    knit(&workspace, ["bundle", "pull", "feature-a"]); // establishes recipe ancestor
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let original: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    let recipe = json!({"merge":{"enabled":false},"onFailure":"stop","steps":[{"id":"inspect","type":"run","repoId":"backend","command":["git","--version"],"effect":"read_only"}]});
    let mut web_project = original.clone();
    web_project["landing"] = recipe.clone();
    fs::write(&project_path, web_project.to_string()).unwrap();
    knit(
        &workspace,
        [
            "--bundle",
            "feature-a",
            "land",
            "plan",
            "--out",
            "web.land.json",
            "--json",
        ],
    );
    let plan: Value =
        serde_json::from_slice(&fs::read(workspace.join("web.land.json")).unwrap()).unwrap();
    let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&plan).unwrap()));
    fs::write(&project_path, original.to_string()).unwrap();
    fs::write(fake_dir.join("landing-recipes.json"), recipe.to_string()).unwrap();
    fs::write(fake_dir.join("landing-artifacts.json"),json!({"data":{"plans":[{"bundleSlug":"feature-a","revision":1,"hash":hash,"plan":plan}],"runs":[]}}).to_string()).unwrap();
    knit(&workspace, ["bundle", "pull", "feature-a"]);
    let pulled: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    assert_eq!(pulled["landing"], recipe);
    let saved = workspace.join(".knit/land-plans/feature-a.land.json");
    // Exact source/recipe fingerprint verification proves the pulled plan is
    // executable against this project, without asking the mock for ownership.
    knit(
        &workspace,
        [
            "--bundle",
            "feature-a",
            "land",
            "validate",
            "--plan",
            saved.to_str().unwrap(),
            "--from-artifact",
            ".knit/bundles/feature-a.bundle.json",
            "--project-file",
            project_path.to_str().unwrap(),
            "--json",
        ],
    );
    let mut local = pulled;
    local["landing"]["steps"][0]["command"] = json!(["git", "status"]);
    fs::write(&project_path, local.to_string()).unwrap();
    let mut remote = recipe.clone();
    remote["steps"][0]["command"] = json!(["git", "--version", "--build-options"]);
    fs::write(fake_dir.join("landing-recipes.json"), remote.to_string()).unwrap();
    let failed = knit_fails(&workspace, ["bundle", "pull", "feature-a"]);
    assert!(failed.contains("Project preserved"), "{failed}");
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&project_path).unwrap()).unwrap(),
        local
    );
    let conflicts = workspace.join(".knit/landing-sync/conflicts");
    assert_eq!(fs::read_dir(conflicts).unwrap().count(), 1);
    let metadata_before = fs::read(fake_dir.join("project-upserts.jsonl")).unwrap_or_default();
    let (stdout, stderr, _) = knit_split_output(
        &workspace,
        &["--bundle", "feature-a", "push", "--remote", "hosted"],
        &[],
    );
    assert_eq!(
        fs::read(fake_dir.join("project-upserts.jsonl")).unwrap_or_default(),
        metadata_before,
        "ordinary push must not PATCH project metadata before recipe CAS"
    );
    assert!(
        format!("{stdout}{stderr}").contains("409"),
        "{stdout}\n{stderr}"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(fake_dir.join("landing-recipes.json")).unwrap())
            .unwrap(),
        remote
    );
    // Reconcile to remote, pull to record its base, then a local recipe edit
    // is safely pushed through CAS by the same ordinary bundle command.
    local["landing"] = remote;
    fs::write(&project_path, local.to_string()).unwrap();
    knit(&workspace, ["bundle", "pull", "feature-a"]);
    local["landing"]["steps"][0]["command"] = json!(["git", "status", "--short"]);
    fs::write(&project_path, local.to_string()).unwrap();
    knit(
        &workspace,
        ["--bundle", "feature-a", "push", "--remote", "hosted"],
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(fake_dir.join("landing-recipes.json")).unwrap())
            .unwrap(),
        local["landing"]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn review_corrections_first_push_preserves_raw_recipe_identity() {
    use serde_json::{json, Value};
    let root = unique_temp_dir();
    let fake = root.join("fake");
    let (export, _) = export_with_feature_bundle(&root);
    let url = spawn_fake_remote_api(&fake, export.to_string());
    let workspace = cloned_workspace(&root, &url);
    knit(&workspace, ["bundle", "pull", "feature-a"]);
    // Start the recipe-capable API as a new project, with no recipe ancestor.
    fs::write(fake.join("initial-project.json"), "{}").unwrap();
    fs::write(
        fake.join("landing-artifacts.json"),
        json!({"data":{"plans":[],"runs":[]}}).to_string(),
    )
    .unwrap();
    let path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let recipe = json!({"steps":[],"deployments":[],"lanes":{"preview":{"branches":{"backend":"preview"},"steps":[],"deployments":[],"extension":{"values":[]}}},"extension":[]});
    project["landing"] = recipe.clone();
    fs::write(path, project.to_string()).unwrap();
    let plan = json!({"kind":"KnitLandPlan","schemaVersion":"0.2","requiredExecutorVersion":"0.3","id":"synthetic","bundleId":"feature-a","lane":"preview","steps":[]});
    fs::create_dir_all(workspace.join(".knit/land-plans")).unwrap();
    fs::write(
        workspace.join(".knit/land-plans/authored.land.json"),
        plan.to_string(),
    )
    .unwrap();
    let (_, stderr, success) = knit_split_output(
        &workspace,
        &["--bundle", "feature-a", "push", "--remote", "hosted"],
        &[],
    );
    assert!(success, "{stderr}");
    let remote: Value =
        serde_json::from_slice(&fs::read(fake.join("landing-recipes.json")).unwrap()).unwrap();
    assert_eq!(remote, recipe);
    let posts = fs::read_to_string(fake.join("landing-posts.jsonl")).unwrap();
    let posted: Value = serde_json::from_str(posts.lines().last().unwrap()).unwrap();
    assert_eq!(posted["plans"][0]["plan"], plan);
    fs::remove_dir_all(root).unwrap();
}
