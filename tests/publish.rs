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
        ["publish", "create", "--github", "--draft"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert!(create.contains("frontend"));
    assert!(create.contains("created"));
    assert!(create.contains("synced"));
    // Multi-repo publishing streams: a header up front, one `pushed` line per
    // repo as its branch reaches origin, and a done/total tail per review.
    assert!(create.contains("publishing 2 repo(s)"), "{create}");
    assert_eq!(create.matches(": pushed ").count(), 2, "{create}");
    assert!(create.contains("(1/2)"), "{create}");
    assert!(create.contains("(2/2)"), "{create}");

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

    let status = knit(&workspace, ["publish", "status", "--github"]);
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
    assert!(land_plan.contains("backend -> main"));
    assert!(land_plan.contains("frontend -> main"));
    assert!(land_plan.contains("knit land apply"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_renew_replaces_a_merged_review_without_replacing_the_bundle() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "continued review"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/continued-review/backend");
    append_line(&feature.join("app.txt"), "first review");
    knit(&workspace, ["commit", "--all", "-m", "First review"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    fs::write(fake_gh_dir.join("merged-backend"), "").unwrap();
    fs::write(fake_gh_dir.join("next-backend.number"), "404").unwrap();
    append_line(&feature.join("app.txt"), "second review");
    knit(&workspace, ["commit", "--all", "-m", "Second review"]);

    let renewed = knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--renew", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(renewed.contains("created"));
    assert!(renewed.contains("#404"));

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/continued-review.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bundle["id"].as_str(), Some("continued-review"));
    assert_eq!(
        bundle["publications"][0]["url"].as_str(),
        Some("https://github.com/acme/backend/pull/404")
    );
    let status = knit(&workspace, ["status"]);
    assert!(status.contains("State: open"));
    assert!(status.contains("not landed"));
    let publication_status = knit(&workspace, ["publish", "status"]);
    assert!(publication_status.contains("not landed"));
    assert!(renewed.contains("knit land plan --force"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_renew_refuses_to_replace_an_open_review() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "open review"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/open-review/backend");
    append_line(&feature.join("app.txt"), "review change");
    knit(&workspace, ["commit", "--all", "-m", "Review change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let failure = knit_fails_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--renew", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(failure.contains("is still open"));
    assert!(!fake_gh_dir.join("next-backend.number").exists());

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
    knit(&workspace, ["commit", "--all", "-m", "Artifact PR change"]);

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
            "create".to_string(),
            "--github".to_string(),
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
fn artifact_pr_create_can_use_native_ipv4_transport() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact publish"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-publish/backend");
    append_line(&backend_feature.join("app.txt"), "artifact PR change");
    knit(&workspace, ["commit", "--all", "-m", "Artifact PR change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let api_base = spawn_fake_github_api(&fake_gh_dir);

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
            "create".to_string(),
            "--github".to_string(),
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
            // The historical curl-era value still selects the (now native)
            // IPv4-first transport.
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
            ("KNIT_GITHUB_API_BASE", api_base.as_str()),
        ],
    );
    assert!(create.contains("backend"));
    assert!(create.contains("created"));
    assert!(!fake_gh_dir.join("api-backend.endpoint").exists());
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("api.authorization"))
            .unwrap()
            .trim(),
        "Bearer gho_fake_token"
    );

    let payload: Value = serde_json::from_str(
        &fs::read_to_string(fake_gh_dir.join("api-backend-create.json")).unwrap(),
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
    knit(&workspace, ["commit", "--all", "-m", "Artifact PR change"]);

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
            "create".to_string(),
            "--github".to_string(),
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
fn pr_create_targets_a_branch_and_a_default_rerun_retargets_back() {
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
            "create",
            "--github",
            "--no-sync",
            "--target",
            "release",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert!(create.contains("created"));
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
    // The bundle's own recorded base is untouched by the destination choice.
    assert_eq!(bundle["repos"][0]["baseBranch"].as_str(), Some("main"));

    let land_plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(land_plan.contains("backend -> release"), "{land_plan}");

    // A rerun with no destination flag asks for the bundle base again, so the
    // open review is retargeted back instead of left on `release`.
    let rerun = knit_with_fake_gh_env(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_DRAFT", "1")],
    );
    assert!(rerun.contains("retargeted"), "{rerun}");
    assert!(rerun.contains("knit land plan --force"), "{rerun}");
    assert!(rerun.contains("release -> main"), "{rerun}");
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("retarget-order.txt")).unwrap(),
        "backend main\n"
    );
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("create-backend.base"))
            .unwrap()
            .trim(),
        "main"
    );

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/release-target.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        bundle["publications"][0]["baseBranch"].as_str(),
        Some("main")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_without_destination_flags_uses_the_bundle_base_branch() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    // An explicitly configured project base is what the bundle records, and
    // the default publish destination follows it — not the remote's `main`.
    git(&backend, ["branch", "release", "main"]);
    git(&backend, ["push", "origin", "release"]);
    knit(&workspace, ["init", "explicit-base"]);
    knit(
        &workspace,
        [
            "project",
            "add",
            "backend",
            backend.to_str().unwrap(),
            "--base",
            "release",
        ],
    );
    knit(&workspace, ["bundle", "explicit base"]);
    let backend_feature = workspace.join(".knit/worktrees/explicit-base/backend");
    append_line(&backend_feature.join("app.txt"), "default base change");
    knit(&workspace, ["commit", "--all", "-m", "Default base change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);

    let create = knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("created"), "{create}");
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("create-backend.base"))
            .unwrap()
            .trim(),
        "release"
    );

    let bundle = read_workspace_bundle(&workspace, "explicit-base");
    assert_eq!(bundle["repos"][0]["baseBranch"].as_str(), Some("release"));
    assert_eq!(
        bundle["publications"][0]["baseBranch"].as_str(),
        Some("release")
    );

    fs::remove_dir_all(root).unwrap();
}

/// A project workspace with backend/frontend/docs repos in a bundle with
/// recorded work in all three, ready for lane publishing.
fn lane_bundle_ready(
    root: &std::path::Path,
    lanes: Value,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(root, "frontend");
    let (_docs_remote, docs, _docs_collaborator) = init_remote_repo(root, "docs");
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
    knit(
        &workspace,
        ["project", "add", "docs", docs.to_str().unwrap()],
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

    knit(&workspace, ["bundle", "lane routing"]);
    for repo_id in ["backend", "frontend", "docs"] {
        append_line(
            &workspace
                .join(".knit/worktrees/lane-routing")
                .join(repo_id)
                .join("app.txt"),
            "lane change",
        );
    }
    knit(&workspace, ["commit", "--all", "-m", "Lane change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    (workspace, fake_bin, fake_gh_dir)
}

fn read_workspace_bundle(workspace: &std::path::Path, slug: &str) -> Value {
    serde_json::from_str(
        &fs::read_to_string(workspace.join(format!(".knit/bundles/{slug}.bundle.json"))).unwrap(),
    )
    .unwrap()
}

#[test]
fn pr_create_lane_routes_per_repo_default_and_null_entries() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = lane_bundle_ready(
        &root,
        json!({
            "staging": {
                "defaultBranch": "staging",
                "branches": { "backend": "staging-backend", "docs": null }
            }
        }),
    );

    // Every repo gets an open review first, so the lane run has to leave the
    // absent repo's existing review and body untouched.
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    for repo in ["backend", "frontend", "docs"] {
        assert!(fake_gh_dir.join(format!("create-{repo}.base")).exists());
    }

    let lane_run = knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        lane_run.contains("not in lane `staging`: docs"),
        "{lane_run}"
    );
    let mut retargets: Vec<String> = fs::read_to_string(fake_gh_dir.join("retarget-order.txt"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    retargets.sort();
    assert_eq!(
        retargets,
        vec![
            "backend staging-backend".to_string(),
            "frontend staging".to_string(),
        ]
    );
    // Body sync covers exactly what the lane published: the absent repo's
    // body is not rewritten.
    assert!(fake_gh_dir.join("edit-backend.md").exists());
    assert!(fake_gh_dir.join("edit-frontend.md").exists());
    assert!(!fake_gh_dir.join("edit-docs.md").exists());
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("create-attempts-docs"))
            .unwrap()
            .lines()
            .count(),
        1
    );

    let bundle = read_workspace_bundle(&workspace, "lane-routing");
    let bases: Vec<&str> = bundle["publications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|publication| {
            let base = publication["baseBranch"].as_str().unwrap();
            let repo = publication["repoId"].as_str().unwrap();
            assert!(base.contains("staging") || repo == "docs");
            base
        })
        .collect();
    assert_eq!(bases.len(), 3);
    let docs_base = bundle["publications"]
        .as_array()
        .unwrap()
        .iter()
        .find(|publication| publication["repoId"] == "docs")
        .unwrap()["baseBranch"]
        .as_str()
        .unwrap();
    assert_eq!(docs_base, "main");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_lane_validation_fails_before_any_push_or_review_write() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = lane_bundle_ready(
        &root,
        json!({
            "staging": { "branches": { "backend": "staging" } },
            "conflicted": {
                "defaultBranch": "staging",
                "branches": { "*": null }
            }
        }),
    );

    // A selected repo the lane never maps is an error, and it must fire
    // before the worker pool starts: nothing is pushed, nothing created.
    let unmapped = knit_fails_with_fake_gh(
        &workspace,
        [
            "publish",
            "create",
            "--github",
            "--no-sync",
            "--lane",
            "staging",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        unmapped.contains("has no branch for repository `frontend`"),
        "{unmapped}"
    );
    assert!(unmapped.contains("declare it absent"), "{unmapped}");
    assert!(!fake_gh_dir.join("create-attempted-backend").exists());
    assert!(!fake_gh_dir.join("create-attempted-frontend").exists());
    assert!(!git_success(
        &root.join("backend.git"),
        ["rev-parse", "refs/heads/knit/lane-routing"],
    ));

    let unknown = knit_fails_with_fake_gh(
        &workspace,
        [
            "publish",
            "create",
            "--github",
            "--no-sync",
            "--lane",
            "nope",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(unknown.contains("Unknown landing lane `nope`"), "{unknown}");

    let conflicted = knit_fails_with_fake_gh(
        &workspace,
        [
            "publish",
            "create",
            "--github",
            "--no-sync",
            "--lane",
            "conflicted",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        conflicted.contains(
            "declares defaultBranch `staging` and a null branches.*, which contradict each other"
        ),
        "{conflicted}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn publish_create_rejects_base_flag_and_conflicting_destination_flags() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "flag guard"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    let base = knit_fails(&workspace, ["publish", "create", "--base", "release"]);
    assert!(base.contains("unexpected argument"), "{base}");
    assert!(base.contains("--base"), "{base}");

    let both = knit_fails(
        &workspace,
        [
            "publish", "create", "--target", "release", "--lane", "staging",
        ],
    );
    assert!(both.contains("cannot be used with"), "{both}");

    let artifact_lane = knit_fails(
        &workspace,
        [
            "publish",
            "create",
            "--from-artifact",
            "bundle.json",
            "--lane",
            "staging",
        ],
    );
    assert!(
        artifact_lane.contains("cannot be used with"),
        "{artifact_lane}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn renew_with_a_changed_target_keeps_the_renewal_contract() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "renew target"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/renew-target/backend");
    append_line(&feature.join("app.txt"), "first review");
    knit(&workspace, ["commit", "--all", "-m", "First review"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    // An open review is never replaced — and never quietly retargeted either.
    let open = knit_fails_with_fake_gh(
        &workspace,
        [
            "publish",
            "create",
            "--github",
            "--no-sync",
            "--renew",
            "--target",
            "release",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(open.contains("is still open"), "{open}");
    assert!(!fake_gh_dir.join("retarget-order.txt").exists());
    assert!(!fake_gh_dir.join("next-backend.number").exists());

    fs::write(fake_gh_dir.join("merged-backend"), "").unwrap();
    fs::write(fake_gh_dir.join("next-backend.number"), "404").unwrap();
    append_line(&feature.join("app.txt"), "second review");
    knit(&workspace, ["commit", "--all", "-m", "Second review"]);

    let renewed = knit_with_fake_gh(
        &workspace,
        [
            "publish",
            "create",
            "--github",
            "--no-sync",
            "--renew",
            "--target",
            "release",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(renewed.contains("created"), "{renewed}");
    assert!(renewed.contains("#404"), "{renewed}");
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("create-backend.base"))
            .unwrap()
            .trim(),
        "release"
    );
    assert!(!fake_gh_dir.join("retarget-order.txt").exists());

    let bundle = read_workspace_bundle(&workspace, "renew-target");
    assert_eq!(
        bundle["publications"][0]["url"].as_str(),
        Some("https://github.com/acme/backend/pull/404")
    );
    assert_eq!(
        bundle["publications"][0]["baseBranch"].as_str(),
        Some("release")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_pr_create_accepts_a_target_branch() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact target"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-target/backend");
    append_line(&backend_feature.join("app.txt"), "artifact PR change");
    knit(&workspace, ["commit", "--all", "-m", "Artifact PR change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);

    let artifact = workspace.join(".knit/bundles/artifact-target.bundle.json");
    let mut artifact_payload: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    artifact_payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&artifact_payload).unwrap(),
    )
    .unwrap();

    let out = root.join("artifact-target.out.bundle.json");
    let create = knit_with_fake_gh(
        &root,
        vec![
            "publish".to_string(),
            "create".to_string(),
            "--github".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
            "--no-push".to_string(),
            "--no-sync".to_string(),
            "--target".to_string(),
            "release".to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("created"), "{create}");

    let payload: Value =
        serde_json::from_str(&fs::read_to_string(fake_gh_dir.join("api-backend.json")).unwrap())
            .unwrap();
    assert_eq!(payload["base"].as_str(), Some("release"));

    let published: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        published["publications"][0]["baseBranch"].as_str(),
        Some("release")
    );

    fs::remove_dir_all(root).unwrap();
}

/// One bundle with one committed repo and a fake `gh` on PATH: the shared
/// setup for the publish-resilience tests below.
fn one_repo_ready_to_publish(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let (_remote, backend, _collaborator) = init_remote_repo(root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    append_line(&feature.join("app.txt"), "publish resilience");
    knit(&workspace, ["commit", "--all", "-m", "Publish resilience"]);
    (workspace, feature)
}

#[test]
fn pr_create_retries_a_host_that_was_briefly_unavailable() {
    let root = unique_temp_dir();
    let (workspace, _feature) = one_repo_ready_to_publish(&root);
    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    fake_gh_fail_create(&fake_gh_dir, "backend", "gh: Bad gateway (HTTP 502)", true);

    let create = knit_with_fake_gh_env(
        &workspace,
        ["publish", "create", "--github"],
        &fake_bin,
        &fake_gh_dir,
        &[("KNIT_RETRY_BASE_MS", "0")],
    );
    assert!(
        create.contains("backend: retrying gh pr create (2/4) after HTTP 502"),
        "{create}"
    );
    assert!(create.contains("created"), "{create}");
    assert_eq!(fake_gh_create_attempts(&fake_gh_dir, "backend"), 2);

    let bundle = read_bundle(&workspace);
    assert_eq!(bundle["publications"].as_array().unwrap().len(), 1);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_does_not_retry_a_refused_credential() {
    let root = unique_temp_dir();
    let (workspace, _feature) = one_repo_ready_to_publish(&root);
    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    fake_gh_fail_create(
        &fake_gh_dir,
        "backend",
        "gh: Bad credentials (HTTP 401)",
        false,
    );

    let create = knit_fails_with_fake_gh_env(
        &workspace,
        ["publish", "create", "--github"],
        &fake_bin,
        &fake_gh_dir,
        &[("KNIT_RETRY_BASE_MS", "0")],
    );
    // GitHub answered. Repeating the call would only repeat the answer.
    assert_eq!(
        fake_gh_create_attempts(&fake_gh_dir, "backend"),
        1,
        "{create}"
    );
    assert!(!create.contains("retrying"), "{create}");
    assert!(create.contains("PR create failed"), "{create}");
    assert!(create.contains("re-run `knit publish create`"), "{create}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_adopts_the_review_a_lost_create_already_made() {
    let root = unique_temp_dir();
    let (workspace, _feature) = one_repo_ready_to_publish(&root);
    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    // The shape of a create whose reply was lost: the PR exists on the host,
    // and the next create is told so.
    fake_gh_existing_after_create(&fake_gh_dir, "backend");
    fake_gh_fail_create(
        &fake_gh_dir,
        "backend",
        "gh: A pull request already exists for acme:knit/venue-capacity. (HTTP 422)",
        false,
    );

    let create = knit_with_fake_gh_env(
        &workspace,
        ["publish", "create", "--github"],
        &fake_bin,
        &fake_gh_dir,
        &[("KNIT_RETRY_BASE_MS", "0")],
    );
    assert!(create.contains("exists"), "{create}");
    assert!(!create.contains("PR create failed"), "{create}");
    // A 422 is an answer, so the create itself is never repeated: the
    // existing review is looked up instead.
    assert_eq!(fake_gh_create_attempts(&fake_gh_dir, "backend"), 1);

    let bundle = read_bundle(&workspace);
    let publications = bundle["publications"].as_array().unwrap();
    assert_eq!(publications.len(), 1);
    assert_eq!(
        publications[0]["url"].as_str(),
        Some("https://github.com/acme/backend/pull/101")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_bounds_forge_writes_and_still_publishes_every_repo() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let names = ["one", "two", "three", "four", "five"];
    let mut paths = Vec::new();
    for name in names {
        let (_remote, repo, _collaborator) = init_remote_repo(&root, name);
        paths.push(repo);
    }
    knit(&workspace, ["bundle", "venue capacity"]);
    let mut add: Vec<String> = vec!["bundle".to_string(), "add".to_string()];
    add.extend(paths.iter().map(|path| path.to_str().unwrap().to_string()));
    knit(&workspace, &add);
    for name in names {
        let feature = workspace.join(format!(".knit/worktrees/venue-capacity/{name}"));
        append_line(&feature.join("app.txt"), "bounded publish");
    }
    knit(&workspace, ["commit", "--all", "-m", "Bounded publish"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);

    let create = knit_with_fake_gh_env(
        &workspace,
        ["publish", "create", "--github"],
        &fake_bin,
        &fake_gh_dir,
        &[("KNIT_FORGE_JOBS", "2")],
    );
    assert!(
        create.contains("publishing 5 repo(s), 2 at a time"),
        "{create}"
    );
    assert_eq!(create.matches(": pushed ").count(), 5, "{create}");
    assert_eq!(create.matches(": created ").count(), 5, "{create}");
    assert!(create.contains("(5/5)"), "{create}");
    for name in names {
        assert_eq!(fake_gh_create_attempts(&fake_gh_dir, name), 1, "{name}");
    }

    let bundle = read_bundle(&workspace);
    assert_eq!(bundle["publications"].as_array().unwrap().len(), 5);

    fs::remove_dir_all(root).unwrap();
}
