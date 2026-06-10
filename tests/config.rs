mod common;

use common::*;
use serde_json::Value;
use std::fs;

#[test]
fn bundle_lifecycle_clean_schema_migrate_doctor_and_advice_work() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "life cycle"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    assert!(knit(&workspace, ["schema", "print", "bundle"]).contains("ChangeGroup"));

    let archive = knit(
        &workspace,
        ["bundle", "archive", "life-cycle", "--reason", "done"],
    );
    assert!(archive.contains("Archived bundle"));
    let archived_bundle_path = workspace.join(".knit/bundles/life-cycle.bundle.json");
    let archived_bundle: Value =
        serde_json::from_str(&fs::read_to_string(&archived_bundle_path).unwrap()).unwrap();
    assert_eq!(archived_bundle["state"].as_str(), Some("archived"));
    assert!(archived_bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["type"].as_str() == Some("feature.archived")));
    // Archive tears down generated worktrees but keeps the feature branch.
    assert!(!workspace
        .join(".knit/worktrees/life-cycle/backend")
        .exists());
    assert!(git(&backend, ["branch", "--list", "knit/life-cycle"]).contains("knit/life-cycle"));

    assert!(!knit(&workspace, ["bundle", "list"]).contains("life-cycle"));
    assert!(knit(&workspace, ["bundle", "list", "--archived"]).contains("archived"));
    assert!(knit_fails(&workspace, ["switch", "life-cycle"]).contains("archived"));
    knit(&workspace, ["bundle", "restore", "life-cycle"]);
    assert!(knit(&workspace, ["bundle", "list"]).contains("open"));

    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    config.as_object_mut().unwrap().remove("advice");
    fs::write(
        workspace.join(".knit/config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&archived_bundle_path).unwrap()).unwrap();
    bundle.as_object_mut().unwrap().remove("state");
    fs::write(
        &archived_bundle_path,
        serde_json::to_string_pretty(&bundle).unwrap(),
    )
    .unwrap();
    assert!(knit_fails(&workspace, ["migrate", "--check"]).contains("need migration"));
    knit(&workspace, ["migrate"]);
    assert!(fs::read_to_string(workspace.join(".knit/config.json"))
        .unwrap()
        .contains("\"advice\": true"));
    // Migration re-infers the archived state from the feature.archived node.
    assert!(fs::read_to_string(&archived_bundle_path)
        .unwrap()
        .contains("\"state\": \"archived\""));

    assert!(knit(&workspace, ["doctor"]).contains("Knit doctor: ok"));
    fs::write(workspace.join(".knit/locks/stale.lock"), "").unwrap();
    assert!(knit_fails(&workspace, ["doctor"]).contains("stale lock"));
    fs::remove_file(workspace.join(".knit/locks/stale.lock")).unwrap();

    knit(&workspace, ["config", "set", "advice", "false"]);
    assert!(fs::read_to_string(workspace.join(".knit/config.json"))
        .unwrap()
        .contains("\"advice\": false"));

    knit(&workspace, ["bundle", "delete", "life-cycle", "--force"]);
    assert!(!archived_bundle_path.exists());
    assert!(workspace
        .join(".knit/deleted/bundles/life-cycle.bundle.json")
        .exists());
    assert!(knit(&workspace, ["bundle", "list", "--deleted"]).contains("deleted"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_config_supports_global_fallback_and_workspace_override() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    let knit_home = root.join("knit-home");
    fs::create_dir_all(&workspace).unwrap();
    let env = [("KNIT_HOME", knit_home.to_str().unwrap())];

    knit_with_env(
        &workspace,
        [
            "remote",
            "add",
            "--global",
            "knithub",
            "https://api.knithub.dev",
            "--token",
            "global-token",
        ],
        &env,
    );
    let global_config: Value =
        serde_json::from_str(&fs::read_to_string(knit_home.join("config.json")).unwrap()).unwrap();
    assert_eq!(
        global_config["remotes"]["knithub"]["url"].as_str(),
        Some("https://api.knithub.dev")
    );

    knit_with_env(&workspace, ["init", "arbient"], &env);
    let inherited = knit_with_env(&workspace, ["remote", "show", "knithub"], &env);
    assert!(inherited.contains("https://api.knithub.dev"));
    assert!(inherited.contains("Scope: global"));
    assert!(inherited.contains("Token: stored"));

    knit_with_env(
        &workspace,
        ["remote", "add", "knithub", "http://localhost:4000"],
        &env,
    );
    let overridden = knit_with_env(&workspace, ["remote", "show", "knithub"], &env);
    assert!(overridden.contains("http://localhost:4000"));
    assert!(overridden.contains("Scope: workspace"));

    let global_only = knit_with_env(&workspace, ["remote", "show", "--global", "knithub"], &env);
    assert!(global_only.contains("https://api.knithub.dev"));
    assert!(global_only.contains("Scope: global"));

    let show = knit_with_env(&workspace, ["config", "show"], &env);
    assert!(show.contains("Global config"));
    assert!(show.contains("Effective config"));
    assert!(show.contains("https://api.knithub.dev"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn clone_resolves_url_and_token_from_global_config_outside_a_workspace() {
    let root = unique_temp_dir();
    // A plain directory with no `.knit` workspace anywhere above it.
    let outside = root.join("outside");
    let knit_home = root.join("knit-home");
    fs::create_dir_all(&outside).unwrap();
    let env = [("KNIT_HOME", knit_home.to_str().unwrap())];

    // Configure a global remote, then clone from a non-workspace directory. An
    // unroutable URL makes the request fail fast once resolution succeeds.
    knit_with_env(
        &outside,
        [
            "remote",
            "add",
            "--global",
            "knithub",
            "http://127.0.0.1:9",
            "--token",
            "global-token",
        ],
        &env,
    );

    let output = knit_fails_with_env(&outside, ["clone", "acme/widgets"], &env);

    // The fix: clone must consult the global config outside a workspace, so it
    // never reports a missing URL or token; it should reach the network step.
    assert!(
        !output.contains("No KnitHub URL configured"),
        "global URL should be used outside a workspace: {output}"
    );
    assert!(
        !output.contains("No KnitHub token configured"),
        "global token should be used outside a workspace: {output}"
    );
    assert!(
        output.contains("KnitHub request failed"),
        "clone should fail at the request step, not resolution: {output}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn config_can_target_multiple_knithub_sync_remotes() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "remote fanout"]);
    knit(
        &workspace,
        ["remote", "add", "local", "http://localhost:4000"],
    );
    knit(&workspace, ["remote", "add", "prod", "https://knithub.dev"]);

    let set = knit(&workspace, ["config", "set", "sync-remotes", "local,prod"]);
    assert!(set.contains("sync-remotes=local,prod"), "{set}");
    let config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    assert_eq!(config["syncRemote"].as_str(), Some("local"));
    assert_eq!(
        config["syncRemotes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["local", "prod"]
    );

    let list = knit(&workspace, ["remote", "list"]);
    assert!(list.contains("local"), "{list}");
    assert!(list.contains("prod"), "{list}");
    assert_eq!(list.matches("sync").count(), 2, "{list}");

    let missing = knit_fails(&workspace, ["config", "set", "sync-remotes", "missing"]);
    assert!(
        missing.contains("No KnitHub remote named `missing`"),
        "{missing}"
    );

    knit(&workspace, ["remote", "remove", "local"]);
    let config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    assert_eq!(config["syncRemote"].as_str(), Some("prod"));
    assert_eq!(
        config["syncRemotes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["prod"]
    );

    fs::remove_dir_all(root).unwrap();
}

