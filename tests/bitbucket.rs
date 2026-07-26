mod common;

use common::{
    append_line, git, init_remote_repo, knit, knit_with_env, knit_with_fake_gh_env,
    spawn_fake_bitbucket_api, unique_temp_dir, write_fake_gh,
};
use knit::providers::{bitbucket::Bitbucket, Forge, PrTarget};
use serde_json::Value;
use std::fs;

#[test]
fn bitbucket_native_adapter_covers_publish_status_retarget_and_land_surfaces() {
    let root = unique_temp_dir();
    let state = root.join("fake-bitbucket");
    let base = spawn_fake_bitbucket_api(&state);
    std::env::set_var("KNIT_BITBUCKET_API_BASE", &base);
    std::env::remove_var("KNIT_BITBUCKET_ACCESS_TOKEN");
    std::env::remove_var("KNIT_BITBUCKET_EMAIL");
    std::env::remove_var("KNIT_BITBUCKET_API_TOKEN");

    let forge = Bitbucket;
    let target = PrTarget::explicit(&root, "acme/backend");
    let auth_error = forge
        .create(&target, "main", "knit/forge", "Forge parity", "body", false)
        .unwrap_err()
        .to_string();
    assert!(auth_error.contains("KNIT_BITBUCKET_ACCESS_TOKEN"));
    assert!(auth_error.contains("KNIT_BITBUCKET_EMAIL"));
    assert!(auth_error.contains("KNIT_BITBUCKET_API_TOKEN"));

    std::env::set_var("KNIT_BITBUCKET_ACCESS_TOKEN", "test-token");
    assert!(forge
        .find_existing(&target, "knit/forge", "main")
        .unwrap()
        .is_none());
    let url = forge
        .create(&target, "main", "knit/forge", "Forge parity", "body", false)
        .unwrap();
    assert_eq!(url, "https://bitbucket.org/acme/backend/pull-requests/101");
    let payload: Value =
        serde_json::from_str(&fs::read_to_string(state.join("bitbucket-create.json")).unwrap())
            .unwrap();
    assert_eq!(payload["source"]["branch"]["name"], "knit/forge");

    forge.edit_base(&target, "101", "staging").unwrap();
    let pr = forge.view(&target, "101").unwrap();
    assert_eq!(pr.base_ref_name.as_deref(), Some("staging"));
    assert_eq!(pr.head_ref_oid.as_deref(), Some("deadbeefcafe"));
    assert_eq!(pr.review_decision.as_deref(), Some("APPROVED"));

    let checks = forge.check_runs(&target, "101", true).unwrap();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].bucket.as_deref(), Some("pass"));

    let mismatch = forge
        .merge(&target, "101", "merge", true, Some("baadf00dcafe"))
        .unwrap_err()
        .to_string();
    assert!(mismatch.contains("does not match expected"));
    forge
        .merge(&target, "101", "squash", true, Some("deadbeefcafebabe"))
        .unwrap();
    let merge: Value =
        serde_json::from_str(&fs::read_to_string(state.join("bitbucket-merge.json")).unwrap())
            .unwrap();
    assert_eq!(merge["merge_strategy"], "squash");
    assert_eq!(merge["close_source_branch"], true);
    assert_eq!(
        fs::read_to_string(state.join("bitbucket.authorization"))
            .unwrap()
            .trim(),
        "Bearer test-token"
    );
    let query = fs::read_to_string(state.join("bitbucket.query")).unwrap();
    assert!(query.contains("source.branch.name%20%3D%20%22knit%2Fforge%22"));

    std::env::remove_var("KNIT_BITBUCKET_API_BASE");
    std::env::remove_var("KNIT_BITBUCKET_ACCESS_TOKEN");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_publish_records_bitbucket_review_and_syncs_body() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "artifact bitbucket"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/artifact-bitbucket/backend");
    append_line(&feature.join("app.txt"), "bitbucket publish");
    knit(&workspace, ["commit", "--all", "-m", "Bitbucket publish"]);

    let artifact = workspace.join(".knit/bundles/artifact-bitbucket.bundle.json");
    let mut bundle: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    bundle["repos"][0]["remote"] =
        Value::String("https://bitbucket.org/acme/backend.git".to_string());
    fs::write(&artifact, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();

    let state = root.join("fake-bitbucket-artifact");
    let base = spawn_fake_bitbucket_api(&state);
    let out = root.join("published.bundle.json");
    let output = knit_with_env(
        &root,
        vec![
            "publish".to_string(),
            "create".to_string(),
            "--provider".to_string(),
            "bitbucket".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
            "--no-push".to_string(),
            "--no-sync".to_string(),
        ],
        &[
            ("KNIT_BITBUCKET_API_BASE", &base),
            ("KNIT_BITBUCKET_ACCESS_TOKEN", "test-token"),
        ],
    );
    assert!(output.contains("created"), "{output}");
    let published: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(published["publications"][0]["provider"], "bitbucket");
    assert_eq!(published["publications"][0]["kind"], "pull_request");
    assert_eq!(
        published["publications"][0]["url"],
        "https://bitbucket.org/acme/backend/pull-requests/101"
    );
    let create: Value =
        serde_json::from_str(&fs::read_to_string(state.join("bitbucket-create.json")).unwrap())
            .unwrap();
    assert!(create["description"]
        .as_str()
        .unwrap()
        .contains("Knit bundle `artifact-bitbucket`"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_publish_status_and_land_apply_archive_through_bitbucket() {
    let root = unique_temp_dir();
    let (remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "bitbucket workspace"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/bitbucket-workspace/backend");
    append_line(&feature.join("app.txt"), "bitbucket workspace");
    knit(
        &workspace,
        ["commit", "--all", "-m", "Bitbucket workspace change"],
    );

    git(
        &feature,
        [
            "remote",
            "set-url",
            "origin",
            "https://bitbucket.org/acme/backend.git",
        ],
    );
    git(
        &feature,
        [
            "remote",
            "set-url",
            "--push",
            "origin",
            remote.to_str().unwrap(),
        ],
    );
    let bundle_path = workspace.join(".knit/bundles/bitbucket-workspace.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    bundle["repos"][0]["remote"] =
        Value::String("https://bitbucket.org/acme/backend.git".to_string());
    fs::write(&bundle_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();

    let state = root.join("fake-bitbucket-workspace");
    let base = spawn_fake_bitbucket_api(&state);
    let env = [
        ("KNIT_BITBUCKET_API_BASE", base.as_str()),
        ("KNIT_BITBUCKET_ACCESS_TOKEN", "workspace-token"),
    ];
    let publish = knit_with_env(
        &workspace,
        [
            "publish",
            "create",
            "--provider",
            "bitbucket",
            "--no-remote",
        ],
        &env,
    );
    assert!(publish.contains("created"), "{publish}");
    let published: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    assert_eq!(published["publications"][0]["provider"], "bitbucket");
    assert_eq!(published["publications"][0]["kind"], "pull_request");

    let edit: Value =
        serde_json::from_str(&fs::read_to_string(state.join("bitbucket-edit.json")).unwrap())
            .unwrap();
    assert!(edit["description"]
        .as_str()
        .unwrap()
        .contains("Knit bundle `bitbucket-workspace`"));

    let status = knit_with_env(
        &workspace,
        ["publish", "status", "--provider", "bitbucket", "--live"],
        &env,
    );
    assert!(status.contains("#101"), "{status}");
    let land_plan = knit_with_env(&workspace, ["land"], &env);
    assert!(land_plan.contains("Provider: bitbucket"), "{land_plan}");
    let landed = knit_with_env(&workspace, ["land", "apply", "--no-remote"], &env);
    assert!(landed.contains("Feature landed"), "{landed}");

    let merge: Value =
        serde_json::from_str(&fs::read_to_string(state.join("bitbucket-merge.json")).unwrap())
            .unwrap();
    assert_eq!(merge["merge_strategy"], "merge_commit");
    assert_eq!(merge["close_source_branch"], false);
    let archived: Value = serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    assert_eq!(archived["state"], "archived");
    assert_eq!(
        archived["publications"][0]["state"],
        Value::String("MERGED".to_string())
    );
    assert!(!feature.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bitbucket_provider_filter_skips_github_and_uses_basic_auth() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "mixed forge filter"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    let backend_feature = workspace.join(".knit/worktrees/mixed-forge-filter/backend");
    let frontend_feature = workspace.join(".knit/worktrees/mixed-forge-filter/frontend");
    append_line(&backend_feature.join("app.txt"), "bitbucket only");
    append_line(&frontend_feature.join("app.txt"), "github untouched");
    knit(&workspace, ["commit", "--all", "-m", "Mixed forge change"]);

    git(
        &backend_feature,
        [
            "remote",
            "set-url",
            "origin",
            "https://bitbucket.org/acme/backend.git",
        ],
    );
    git(
        &backend_feature,
        [
            "remote",
            "set-url",
            "--push",
            "origin",
            backend_remote.to_str().unwrap(),
        ],
    );
    let bundle_path = workspace.join(".knit/bundles/mixed-forge-filter.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    bundle["repos"][0]["remote"] =
        Value::String("https://bitbucket.org/acme/backend.git".to_string());
    bundle["repos"][1]["remote"] =
        Value::String("https://github.com/acme/frontend.git".to_string());
    fs::write(&bundle_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();

    let bitbucket_state = root.join("fake-bitbucket-filter");
    let bitbucket_base = spawn_fake_bitbucket_api(&bitbucket_state);
    let fake_bin = root.join("fake-bin");
    let fake_gh = root.join("fake-gh");
    write_fake_gh(&fake_bin, &fake_gh);
    let output = knit_with_fake_gh_env(
        &workspace,
        [
            "publish",
            "create",
            "--provider",
            "bitbucket",
            "--no-sync",
            "--no-remote",
        ],
        &fake_bin,
        &fake_gh,
        &[
            ("KNIT_BITBUCKET_API_BASE", bitbucket_base.as_str()),
            ("KNIT_BITBUCKET_ACCESS_TOKEN", ""),
            ("KNIT_BITBUCKET_EMAIL", "user@example.test"),
            ("KNIT_BITBUCKET_API_TOKEN", "api-token"),
        ],
    );
    assert!(output.contains("backend"), "{output}");
    assert!(!output.contains("frontend"), "{output}");
    assert!(bitbucket_state.join("bitbucket-create.json").exists());
    assert!(!fake_gh.join("create-frontend.args").exists());
    assert!(!fake_gh.join("api-frontend.json").exists());

    let recorded: Value = serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    assert_eq!(recorded["publications"].as_array().unwrap().len(), 1);
    assert_eq!(recorded["publications"][0]["repoId"], "backend");
    assert_eq!(recorded["publications"][0]["provider"], "bitbucket");
    assert_eq!(
        fs::read_to_string(bitbucket_state.join("bitbucket.authorization"))
            .unwrap()
            .trim(),
        "Basic dXNlckBleGFtcGxlLnRlc3Q6YXBpLXRva2Vu"
    );
    assert!(git(
        &frontend,
        ["ls-remote", "origin", "refs/heads/knit/mixed-forge-filter"],
    )
    .trim()
    .is_empty());
    fs::remove_dir_all(root).unwrap();
}
