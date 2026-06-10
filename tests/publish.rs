mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;

#[test]
fn pr_create_pushes_creates_records_and_syncs_cross_links() {
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
    append_line(&backend_feature.join("app.txt"), "backend PR change");
    append_line(&frontend_feature.join("app.txt"), "frontend PR change");
    knit(&workspace, ["commit", "--all", "-m", "PR change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);

    let create = knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--draft"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert!(create.contains("frontend"));
    assert!(create.contains("created"));
    assert!(create.contains("synced"));

    assert_eq!(
        git(
            &backend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        git(&backend_feature, ["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(
            &frontend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        git(&frontend_feature, ["rev-parse", "HEAD"])
    );

    let bundle = read_bundle(&workspace);
    let publications = bundle["publications"].as_array().unwrap();
    assert_eq!(publications.len(), 2);
    assert_eq!(publications[0]["provider"].as_str(), Some("github"));
    assert_eq!(publications[0]["kind"].as_str(), Some("pull_request"));
    assert!(publications
        .iter()
        .any(|publication| publication["url"] == "https://github.com/acme/backend/pull/101"));
    assert!(publications
        .iter()
        .any(|publication| publication["url"] == "https://github.com/acme/frontend/pull/202"));

    let backend_body = fs::read_to_string(fake_gh_dir.join("edit-backend.md")).unwrap();
    assert!(backend_body.contains("This PR is part of Knit bundle `venue-capacity`."));
    assert!(backend_body.contains("`backend`: https://github.com/acme/backend/pull/101 (this PR)"));
    assert!(backend_body.contains("`frontend`: https://github.com/acme/frontend/pull/202"));

    let frontend_body = fs::read_to_string(fake_gh_dir.join("edit-frontend.md")).unwrap();
    assert!(frontend_body.contains("`backend`: https://github.com/acme/backend/pull/101"));
    assert!(
        frontend_body.contains("`frontend`: https://github.com/acme/frontend/pull/202 (this PR)")
    );

    let status = knit(&workspace, ["publish", "github", "status"]);
    assert!(status.contains("#101"));
    assert!(status.contains("#202"));
    assert!(status.contains("not landed"));
    assert!(status.contains("Next:"));
    assert!(status.contains("knit land"));

    let knit_status = knit(&workspace, ["status"]);
    assert!(knit_status.contains("Publications:"));
    assert!(knit_status.contains("not landed"));
    assert!(knit_status.contains("knit land"));

    let land_plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(land_plan.contains("Lands into:"));
    assert!(land_plan.contains("review object's base branch"));
    assert!(land_plan.contains("knit land apply"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_pr_create_uses_github_api_without_checkout_prompt() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact publish"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-publish/backend");
    append_line(&backend_feature.join("app.txt"), "artifact PR change");
    knit(
        &workspace,
        ["commit", "--all", "-m", "Artifact PR change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);

    let artifact = workspace.join(".knit/bundles/artifact-publish.bundle.json");
    let mut artifact_payload: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    artifact_payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&artifact_payload).unwrap(),
    )
    .unwrap();

    let out = root.join("artifact-publish.out.bundle.json");
    let create = knit_with_fake_gh(
        &root,
        vec![
            "publish".to_string(),
            "github".to_string(),
            "create".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
            "--no-push".to_string(),
            "--no-sync".to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert!(create.contains("created"));
    assert!(!fake_gh_dir.join("create-backend.args").exists());

    let find_endpoint = fs::read_to_string(fake_gh_dir.join("api-backend-find.endpoint")).unwrap();
    assert_eq!(
        find_endpoint.trim(),
        "repos/acme/backend/pulls?state=all&head=acme%3Aknit%2Fartifact-publish&base=main&per_page=1"
    );
    let endpoint = fs::read_to_string(fake_gh_dir.join("api-backend.endpoint")).unwrap();
    assert_eq!(endpoint.trim(), "repos/acme/backend/pulls");
    let prompt = fs::read_to_string(fake_gh_dir.join("api-backend.prompt")).unwrap();
    assert_eq!(prompt.trim(), "1");

    let payload: Value =
        serde_json::from_str(&fs::read_to_string(fake_gh_dir.join("api-backend.json")).unwrap())
            .unwrap();
    assert_eq!(payload["base"].as_str(), Some("main"));
    assert_eq!(payload["head"].as_str(), Some("knit/artifact-publish"));
    assert_eq!(
        payload["title"].as_str(),
        Some("artifact publish (backend)")
    );
    assert!(payload["body"]
        .as_str()
        .unwrap()
        .contains("This PR is part of Knit bundle `artifact-publish`."));

    let published: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        published["publications"][0]["url"].as_str(),
        Some("https://github.com/acme/backend/pull/101")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_pr_create_can_use_curl_ipv4_transport() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact publish"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-publish/backend");
    append_line(&backend_feature.join("app.txt"), "artifact PR change");
    knit(
        &workspace,
        ["commit", "--all", "-m", "Artifact PR change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    write_fake_curl(&fake_bin, &fake_gh_dir);

    let artifact = workspace.join(".knit/bundles/artifact-publish.bundle.json");
    let mut artifact_payload: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    artifact_payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&artifact_payload).unwrap(),
    )
    .unwrap();

    let out = root.join("artifact-publish.out.bundle.json");
    let create = knit_with_fake_gh_env(
        &root,
        vec![
            "publish".to_string(),
            "github".to_string(),
            "create".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
            "--no-push".to_string(),
            "--no-sync".to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &[
            ("GH_TOKEN", "gho_fake_token"),
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
        ],
    );
    assert!(create.contains("backend"));
    assert!(create.contains("created"));
    assert!(!fake_gh_dir.join("api-backend.endpoint").exists());
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("curl.ipv4"))
            .unwrap()
            .trim(),
        "1"
    );
    assert!(fs::read_to_string(fake_gh_dir.join("curl.netrc"))
        .unwrap()
        .contains("password gho_fake_token"));

    let payload: Value = serde_json::from_str(
        &fs::read_to_string(fake_gh_dir.join("curl-backend-create.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(payload["base"].as_str(), Some("main"));
    assert_eq!(payload["head"].as_str(), Some("knit/artifact-publish"));

    let published: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        published["publications"][0]["url"].as_str(),
        Some("https://github.com/acme/backend/pull/101")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_pr_create_reuses_existing_pr_found_with_github_api() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact publish"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-publish/backend");
    append_line(&backend_feature.join("app.txt"), "artifact PR change");
    knit(
        &workspace,
        ["commit", "--all", "-m", "Artifact PR change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    fs::write(fake_gh_dir.join("existing-backend"), "").unwrap();

    let artifact = workspace.join(".knit/bundles/artifact-publish.bundle.json");
    let mut artifact_payload: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    artifact_payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&artifact_payload).unwrap(),
    )
    .unwrap();

    let out = root.join("artifact-publish.out.bundle.json");
    let create = knit_with_fake_gh(
        &root,
        vec![
            "publish".to_string(),
            "github".to_string(),
            "create".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
            "--no-push".to_string(),
            "--no-sync".to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert!(create.contains("exists"));
    assert!(!fake_gh_dir.join("api-backend.json").exists());
    assert!(!fake_gh_dir.join("create-backend.args").exists());

    let find_endpoint = fs::read_to_string(fake_gh_dir.join("api-backend-find.endpoint")).unwrap();
    assert_eq!(
        find_endpoint.trim(),
        "repos/acme/backend/pulls?state=all&head=acme%3Aknit%2Fartifact-publish&base=main&per_page=1"
    );

    let published: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        published["publications"][0]["url"].as_str(),
        Some("https://github.com/acme/backend/pull/101")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_can_override_base_branch() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "release target"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/release-target/backend");
    append_line(&backend_feature.join("app.txt"), "release PR change");
    knit(&workspace, ["commit", "--all", "-m", "Release PR change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);

    let create = knit_with_fake_gh(
        &workspace,
        [
            "publish",
            "github",
            "create",
            "--no-sync",
            "--base",
            "release",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("create-backend.base"))
            .unwrap()
            .trim(),
        "release"
    );

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/release-target.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        bundle["publications"][0]["baseBranch"].as_str(),
        Some("release")
    );

    let rerun = knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(rerun.contains("exists"));

    fs::remove_dir_all(root).unwrap();
}

