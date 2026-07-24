mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

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
            "hosted",
            "https://remote.example.invalid",
            "--token",
            "global-token",
        ],
        &env,
    );
    let global_config: Value =
        serde_json::from_str(&fs::read_to_string(knit_home.join("config.json")).unwrap()).unwrap();
    assert_eq!(
        global_config["remotes"]["hosted"]["url"].as_str(),
        Some("https://remote.example.invalid")
    );

    knit_with_env(&workspace, ["init", "demo"], &env);
    let inherited = knit_with_env(&workspace, ["remote", "show", "hosted"], &env);
    assert!(inherited.contains("https://remote.example.invalid"));
    assert!(inherited.contains("Scope: global"));
    assert!(inherited.contains("Token: stored"));

    knit_with_env(
        &workspace,
        ["remote", "add", "hosted", "http://localhost:4000"],
        &env,
    );
    let overridden = knit_with_env(&workspace, ["remote", "show", "hosted"], &env);
    assert!(overridden.contains("http://localhost:4000"));
    assert!(overridden.contains("Scope: workspace"));

    let global_only = knit_with_env(&workspace, ["remote", "show", "--global", "hosted"], &env);
    assert!(global_only.contains("https://remote.example.invalid"));
    assert!(global_only.contains("Scope: global"));

    let show = knit_with_env(&workspace, ["config", "show"], &env);
    assert!(show.contains("Global config"));
    assert!(show.contains("Effective config"));
    assert!(show.contains("https://remote.example.invalid"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn environment_token_uses_stdin_and_private_global_config_with_arbitrary_remote_name() {
    let root = unique_temp_dir();
    let outside = root.join("outside");
    let knit_home = root.join("knit-home");
    let fake_dir = root.join("fake-remote");
    fs::create_dir_all(&outside).unwrap();
    let remote_url = spawn_fake_remote_api(&fake_dir, String::new());

    let mut child = Command::new(env!("CARGO_BIN_EXE_knit"))
        .args([
            "remote",
            "add",
            "moonbase",
            remote_url.as_str(),
            "--global",
            "--token-stdin",
        ])
        .current_dir(&outside)
        .env("KNIT_HOME", &knit_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"environment-bound-secret\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("environment-bound-secret"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("environment-bound-secret"));

    let config_path = knit_home.join("config.json");
    let config: Value = serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(
        config["remotes"]["moonbase"]["token"].as_str(),
        Some("environment-bound-secret")
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&knit_home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let mut unsupported = Command::new(env!("CARGO_BIN_EXE_knit"))
        .args(["git-credential", "--remote", "moonbase", "get"])
        .current_dir(&outside)
        .env("KNIT_HOME", &knit_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    unsupported
        .stdin
        .take()
        .unwrap()
        .write_all(b"protocol=http\nhost=knit.example.test\n\n")
        .unwrap();
    let unsupported = unsupported.wait_with_output().unwrap();
    assert!(unsupported.status.success());
    assert!(unsupported.stdout.is_empty());

    // A checkout is untrusted input to the credential helper. Even a local
    // remote with the same name must not replace the private global endpoint.
    let knit_home_value = knit_home.to_string_lossy().to_string();
    let private_env = [("KNIT_HOME", knit_home_value.as_str())];
    knit_with_env(&outside, ["bundle", "hostile workspace"], &private_env);
    knit_with_env(
        &outside,
        [
            "remote",
            "add",
            "moonbase",
            "http://127.0.0.1:1",
            "--token",
            "workspace-controlled-token",
        ],
        &private_env,
    );

    let mut supported = Command::new(env!("CARGO_BIN_EXE_knit"))
        .args(["git-credential", "--remote", "moonbase", "get"])
        .current_dir(&outside)
        .env("KNIT_HOME", &knit_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    supported
        .stdin
        .take()
        .unwrap()
        .write_all(b"protocol=https\nhost=code.example.test\npath=alice/project.git\n\n")
        .unwrap();
    let supported = supported.wait_with_output().unwrap();
    assert!(
        supported.status.success(),
        "{}",
        String::from_utf8_lossy(&supported.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&supported.stdout),
        "username=forge-user\npassword=forge-secret\n\n"
    );
    let requests = fs::read_to_string(fake_dir.join("vend-requests.txt")).unwrap();
    assert!(requests.contains("\"host\":\"code.example.test\""));
    assert!(!requests.contains("environment-bound-secret"));
    assert!(!requests.contains("forge-secret"));

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
            "hosted",
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
        !output.contains("No URL configured"),
        "global URL should be used outside a workspace: {output}"
    );
    assert!(
        !output.contains("No remote token configured"),
        "global token should be used outside a workspace: {output}"
    );
    assert!(
        output.contains("Remote request failed"),
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
    knit(
        &workspace,
        ["remote", "add", "prod", "https://prod.example.invalid"],
    );

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
    assert!(missing.contains("No remote named `missing`"), "{missing}");

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

#[test]
fn workspace_scoped_tokens_warn_about_shared_config() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "venue capacity"]);

    knit(&workspace, ["remote", "add", "hub", "http://localhost:9"]);
    let stored = knit(&workspace, ["remote", "token", "hub", "kht_secret"]);
    assert!(stored.contains("warning:"));
    assert!(stored.contains("KNIT_REMOTE_HUB_TOKEN"));

    // Global storage is the recommended scope and stays quiet. Use a private
    // KNIT_HOME: the shared isolated home would leak this global remote into
    // sibling tests' `remote list` output.
    let private_home = root.join("private-knit-home");
    fs::create_dir_all(&private_home).unwrap();
    let env_home = private_home.to_string_lossy().to_string();
    let env = [("KNIT_HOME", env_home.as_str())];
    knit_with_env(
        &workspace,
        ["remote", "add", "ghub", "http://localhost:9", "--global"],
        &env,
    );
    let global_store = knit_with_env(
        &workspace,
        ["remote", "token", "ghub", "kht_secret", "--global"],
        &env,
    );
    assert!(!global_store.contains("warning:"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn every_configured_remote_is_a_sync_remote_by_default() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "default sync set"]);
    knit(
        &workspace,
        ["remote", "add", "alpha", "http://localhost:4000"],
    );
    knit(
        &workspace,
        ["remote", "add", "beta", "https://beta.example.invalid"],
    );

    // No sync-remotes config: the remotes list itself is the sync set.
    let list = knit(&workspace, ["remote", "list"]);
    assert!(!list.contains("not sync"), "{list}");
    assert_eq!(list.matches("sync").count(), 2, "{list}");

    // An explicit sync-remotes narrows the set.
    knit(&workspace, ["config", "set", "sync-remotes", "beta"]);
    let list = knit(&workspace, ["remote", "list"]);
    assert!(list.contains("not sync"), "{list}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_helpers_owns_global_git_credential_helper_entries() {
    let root = unique_temp_dir();
    let outside = root.join("outside");
    let knit_home = root.join("knit-home");
    let gitconfig = root.join("gitconfig");
    let fake_dir = root.join("fake-remote");
    fs::create_dir_all(&outside).unwrap();
    let remote_url = spawn_fake_remote_api(&fake_dir, String::new());

    // Pre-existing global entries: a foreign helper on the connected forge
    // host, and a stale knit-shaped entry left by a renamed remote.
    fs::write(
        &gitconfig,
        concat!(
            "[credential \"https://code.example.test\"]\n",
            "\thelper = osxkeychain\n",
            "[credential \"https://old.example.test\"]\n",
            "\thelper = !'/old/knit' git-credential --remote 'oldname'\n",
        ),
    )
    .unwrap();

    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_knit"))
            .args(args)
            .current_dir(&outside)
            .env("KNIT_HOME", &knit_home)
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "knit {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    };
    let helpers_for = |host: &str| -> Vec<String> {
        let output = Command::new("git")
            .args([
                "config",
                "--file",
                gitconfig.to_str().unwrap(),
                "--get-all",
                &format!("credential.https://{host}.helper"),
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(ToString::to_string)
            .collect()
    };

    run(&[
        "remote",
        "add",
        "hub",
        remote_url.as_str(),
        "--global",
        "--token",
        "hub-secret",
    ]);
    let synced = run(&["remote", "sync-helpers", "hub"]);
    assert!(synced.contains("code.example.test"), "{synced}");

    let connected = helpers_for("code.example.test");
    assert_eq!(connected.len(), 2, "{connected:?}");
    assert!(
        connected[0].contains("git-credential --remote 'hub'"),
        "our helper must lead: {connected:?}"
    );
    assert_eq!(connected[1], "osxkeychain", "foreign helper preserved");
    assert!(
        helpers_for("old.example.test").is_empty(),
        "stale knit entry for a renamed remote must be removed"
    );
    assert!(
        helpers_for("off.example.test").is_empty(),
        "disconnected forges get no helper"
    );

    run(&["remote", "remove", "hub", "--global"]);
    assert_eq!(
        helpers_for("code.example.test"),
        vec!["osxkeychain".to_string()],
        "removal must drop our helper and keep foreign ones"
    );

    fs::remove_dir_all(root).unwrap();
}
