//! Remote clone integration with project-defined auth requirements: the
//! bootstrap that lands before any private repository fetch, recovery of a
//! clone whose private repositories failed for missing credentials, and the
//! pending known-repos map for membership a scope left out.
//!
//! Network is limited to local fixtures: the fake sync remote is a loopback
//! HTTP server, "private" forge repositories use reserved `.example` hosts
//! (NXDOMAIN, fail fast, no real forge), and the recovery phase rewrites the
//! reserved URL to a local bare repository through Git's `insteadOf` in an
//! isolated global config, standing in for the access a linked credential
//! grants. No real tokens or networks are involved.

mod common;

use common::*;
use std::fs;
use std::path::Path;

use common::project_auth::*;

/// A clone whose private repository fails for a missing credential keeps the
/// repository as a project entry, keeps the auth requirements, and is
/// recoverable in place: link a credential (scriptable setup), and the next
/// remote pull re-clones the missing checkout through the binding.
#[test]
fn failed_private_clone_stays_in_project_and_recovers_via_auth_and_pull() {
    let root = unique_temp_dir();
    let backend = root.join("backend-source");
    init_repo(&backend, "backend");
    let private_url = "https://forge.example/org/private.git";
    let export = export_body(
        &[
            ("backend".to_string(), backend.to_string_lossy().to_string()),
            ("private".to_string(), private_url.to_string()),
        ],
        Some(serde_json::json!({"groups": [auth_group("gh", "forge.example", &["private"])]})),
    );
    let base_url = spawn_fake_remote_with_body(export.to_string());
    let target = root.join("workspace");
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();

    // Noninteractive clone: one public repo clones, the private one cannot.
    let output = knit_with_env(
        &root,
        clone_args(&target, &base_url, &[]),
        &[("KNIT_HOME", home.to_str().unwrap())],
    );
    assert!(output.contains("Auth requirements:"), "{output}");
    assert!(output.contains("1 credential group(s)"), "{output}");
    assert!(output.contains("Skipped:"), "{output}");
    assert!(output.contains("private"), "{output}");

    // The workspace is complete and recoverable: project with the full group
    // and an entry for the failed repo, config with the chosen sync remote.
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["auth"]["groups"][0]["repos"],
        serde_json::json!(["private"])
    );
    let ids: Vec<&str> = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["backend", "private"]);
    let private_entry = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|repo| repo["id"] == "private")
        .unwrap();
    assert_eq!(
        private_entry["path"].as_str().unwrap(),
        target.join("private").to_string_lossy()
    );
    let config = read_json(&target.join(".knit/config.json"));
    assert_eq!(config["remotes"]["hosted"]["url"], base_url);
    assert_eq!(config["syncRemotes"], serde_json::json!(["hosted"]));
    // In-scope failures are project entries, never pending-only: the failed
    // repo must stay bindable by `knit auth setup`.
    assert!(!target.join(".knit/projects/demo.known-repos.json").exists());
    // Nothing was assigned silently.
    assert!(!home.join("forge-auth.json").exists());

    // Recovery, executed: scriptable setup binds the failed repo's group...
    let assigned = assign_test_credential(&target, &home, "private");
    assert!(
        assigned.contains("Assigned `ci` to private"),
        "auth use must bind the failed repo's entry: {assigned}"
    );
    let bindings = read_json(&home.join("forge-auth.json"));
    let bound: Vec<&str> = bindings["projects"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|bindings| bindings.as_object().unwrap().keys())
        .map(String::as_str)
        .collect();
    assert_eq!(bound, vec!["private"]);

    // ...and the retried pull clones the checkout through the binding. The
    // insteadOf rewrite stands in for the access the credential grants on the
    // real forge; the binding resolution itself runs for real.
    let bare = make_bare_named(&root, "private");
    let config = instead_of_config(&root, private_url, &bare);
    let pull = knit_with_env(
        &target,
        ["pull", "--bundles"],
        &[
            ("KNIT_HOME", home.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", config.to_str().unwrap()),
            ("KNIT_TEST_TOKEN", "recovered-secret"),
        ],
    );
    assert!(
        pull.contains("recovered") && pull.contains("private"),
        "pull must retry the missing checkout: {pull}"
    );
    assert!(target.join("private").join(".git").exists());
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    let private_entry = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|repo| repo["id"] == "private")
        .unwrap();
    assert!(Path::new(private_entry["path"].as_str().unwrap()).is_dir());

    fs::remove_dir_all(root).unwrap();
}

/// A clone where nothing could be cloned fails, but leaves a workspace valid
/// enough to recover from instead of dead-ending: re-entering `knit clone`
/// into the created target is refused, and the message says what to run.
#[test]
fn clone_with_no_cloneable_repo_bails_but_leaves_a_recoverable_workspace() {
    let root = unique_temp_dir();
    let private_url = "https://forge.example/org/private.git";
    let export = export_body(
        &[("private".to_string(), private_url.to_string())],
        Some(serde_json::json!({"groups": [auth_group("gh", "forge.example", &["private"])]})),
    );
    let base_url = spawn_fake_remote_with_body(export.to_string());
    let target = root.join("workspace");

    let output = knit_fails(&root, clone_args(&target, &base_url, &[]));
    assert!(
        output.contains("Failed to clone any repository"),
        "{output}"
    );
    assert!(
        output.contains("knit auth setup") && output.contains("knit pull --bundles"),
        "the failure must name the recovery commands: {output}"
    );
    assert!(output.contains("recoverable workspace"), "{output}");

    // The workspace exists and carries everything recovery needs: the chosen
    // sync remote, the project with its auth requirements, and the failed
    // repo's entry.
    let config = read_json(&target.join(".knit/config.json"));
    assert_eq!(config["remotes"]["hosted"]["url"], base_url);
    assert_eq!(config["activeProject"], "demo");
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["auth"]["groups"][0]["repos"],
        serde_json::json!(["private"])
    );
    assert_eq!(
        project["repos"][0]["remote"],
        serde_json::json!(private_url)
    );

    // A retry of `knit clone` into the same target refuses (no clobbering),
    // which is why the failure message must direct recovery through the
    // workspace instead.
    let retry = knit_fails(&root, clone_args(&target, &base_url, &[]));
    assert!(
        retry.contains("already a Knit workspace"),
        "retry must refuse the existing target: {retry}"
    );

    fs::remove_dir_all(root).unwrap();
}

/// A scoped clone keeps the remote's full group mappings (an out-of-scope
/// group repo stays in the group), records the out-of-scope membership in the
/// pending known-repos map, and never asks for an out-of-scope credential.
#[test]
fn scoped_clone_preserves_full_groups_and_pends_only_out_of_scope() {
    let root = unique_temp_dir();
    let backend = root.join("backend-source");
    init_repo(&backend, "backend");
    let frontend_url = "https://forge.example/org/frontend.git";
    let export = export_body(
        &[
            ("backend".to_string(), backend.to_string_lossy().to_string()),
            ("frontend".to_string(), frontend_url.to_string()),
        ],
        Some(serde_json::json!({"groups": [auth_group("gh", "forge.example", &["frontend"])]})),
    );
    let base_url = spawn_fake_remote_with_body(export.to_string());
    let target = root.join("workspace");
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();

    let output = knit_with_env(
        &root,
        clone_args(&target, &base_url, &["--repo", "backend"]),
        &[("KNIT_HOME", home.to_str().unwrap())],
    );
    assert!(output.contains("Auth requirements:"), "{output}");

    // Local projection: only the selected repo is a project entry...
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    let ids: Vec<&str> = project["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["backend"]);
    // ...while the group mapping is preserved in full.
    assert_eq!(
        project["auth"]["groups"][0]["repos"],
        serde_json::json!(["frontend"])
    );
    // Out-of-scope membership is the pending map, distinguishable from the
    // in-scope failures that stay in the project.
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({"frontend": frontend_url})
    );
    // No credential was requested or assigned for the out-of-scope group.
    assert!(!home.join("forge-auth.json").exists());

    fs::remove_dir_all(root).unwrap();
}

/// `--prefer-https` rewrites the export's repository URLs after the scoped
/// selection was made; the clone, the persisted project entries, and the
/// pending map must all carry the rewritten HTTPS URL, not the SSH form the
/// probe replaced. Asserts the actual clone URL (the checkout's origin), not
/// a printed message.
#[test]
fn prefer_https_rewrites_the_urls_a_scoped_clone_uses_and_persists() {
    let root = unique_temp_dir();
    // `spawn_fake_remote_api` reports `code.example.test` as a connected
    // forge host, which is what gates the ssh->https rewrite.
    let backend_ssh = "git@code.example.test:org/backend.git";
    let backend_https = "https://code.example.test/org/backend.git";
    let frontend_ssh = "git@code.example.test:org/frontend.git";
    let frontend_https = "https://code.example.test/org/frontend.git";

    let export = export_body(
        &[
            ("backend".to_string(), backend_ssh.to_string()),
            ("frontend".to_string(), frontend_ssh.to_string()),
        ],
        None,
    );
    let fake_dir = root.join("fake-remote");
    let base_url = spawn_fake_remote_api(&fake_dir, export.to_string());

    // Two local bare repos stand in for the forge: the SSH probes fail (no
    // such host), the HTTPS probes succeed because the isolated global git
    // config rewrites them onto the bare repositories.
    let backend_bare = make_bare_named(&root, "backend-bare");
    let frontend_bare = make_bare_named(&root, "frontend-bare");
    let gitconfig = root.join("prefer-https.gitconfig");
    // Serialize through `git config` so Git escapes the subsection itself
    // (a hand-written `[url "C:\..."]` loses its backslashes on parse).
    for (bare, https) in [
        (backend_bare, backend_https),
        (frontend_bare, frontend_https),
    ] {
        git(
            &root,
            [
                "config",
                "--file",
                gitconfig.to_str().unwrap(),
                "--replace-all",
                &format!("url.{}.insteadOf", bare.display()),
                https,
            ],
        );
    }

    let target = root.join("workspace");
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();
    let output = knit_with_env(
        &root,
        clone_args(&target, &base_url, &["--repo", "backend", "--prefer-https"]),
        &[
            ("KNIT_HOME", home.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", gitconfig.to_str().unwrap()),
        ],
    );
    assert!(output.contains("Scope:"), "{output}");

    // The clone actually used the rewritten URL: git records the requested
    // URL as the checkout's origin (insteadOf only affects transport).
    let origin = git(&target.join("backend"), ["remote", "get-url", "origin"]);
    assert_eq!(origin.trim(), backend_https);
    // ...and so does the persisted project entry.
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    assert_eq!(
        project["repos"][0]["remote"],
        serde_json::json!(backend_https)
    );
    // The out-of-scope repo's pending entry carries the rewritten URL too.
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({"frontend": frontend_https})
    );

    fs::remove_dir_all(root).unwrap();
}

/// A scoped clone whose every selected repo fails (all private, no
/// credential) bails, but the recovery promise holds: the scope view is
/// saved locally before any fetch, the full auth groups and the pending map
/// survive, and binding a credential followed by `knit pull --bundles`
/// recovers the repo inside the scope without ever touching the
/// out-of-scope one.
#[test]
fn scoped_all_failed_clone_records_scope_view_and_recovers() {
    let root = unique_temp_dir();
    let backend = root.join("backend-source");
    init_repo(&backend, "backend");
    let private_url = "https://forge.example/org/private.git";
    let frontend_url = "https://forge.example/org/frontend.git";
    let export = export_body(
        &[
            ("backend".to_string(), backend.to_string_lossy().to_string()),
            ("private".to_string(), private_url.to_string()),
            ("frontend".to_string(), frontend_url.to_string()),
        ],
        Some(serde_json::json!({"groups": [
            auth_group("gh", "forge.example", &["private"]),
            auth_group("gh-out", "forge.example", &["frontend"]),
        ]})),
    );
    let base_url = spawn_fake_remote_with_body(export.to_string());
    let target = root.join("workspace");
    let home = root.join("knit-home");
    fs::create_dir_all(&home).unwrap();
    let home_env = ("KNIT_HOME", home.to_str().unwrap());

    let output = knit_fails_with_env(
        &root,
        clone_args(&target, &base_url, &["--repo", "private"]),
        &[home_env],
    );
    assert!(
        output.contains("Failed to clone any repository"),
        "{output}"
    );
    assert!(output.contains("knit pull --bundles"), "{output}");

    // The scope view was saved before the fetch: without it the recovery
    // pull cannot resolve the workspace's scope and skips every retry.
    let views = read_json(&target.join(".knit/views/demo.views.json"));
    assert_eq!(
        views["views"]["scope"]["include"],
        serde_json::json!(["private"])
    );
    assert_eq!(views["views"]["scope"]["base"], serde_json::json!("none"));
    let config = read_json(&target.join(".knit/config.json"));
    assert_eq!(config["scopeView"], serde_json::json!("scope"));

    // Full auth groups survive locally, and the out-of-scope membership
    // (including the other group's repo) is the pending map.
    let project = read_json(&target.join(".knit/projects/demo.project.json"));
    let group_repos: Vec<&str> = project["auth"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["repos"][0].as_str().unwrap())
        .collect();
    assert_eq!(group_repos, vec!["private", "frontend"]);
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({
            "backend": backend.to_string_lossy(),
            "frontend": frontend_url,
        })
    );

    // Recovery, executed: bind the failed repo and pull inside the scope.
    let assigned = assign_test_credential(&target, &home, "private");
    assert!(
        assigned.contains("Assigned `ci` to private"),
        "setup must bind the scoped failed repo: {assigned}"
    );
    let bare = make_bare_named(&root, "private-bare");
    let gitconfig = instead_of_config(&root, private_url, &bare);
    let pull = knit_with_env(
        &target,
        ["pull", "--bundles"],
        &[
            home_env,
            ("GIT_CONFIG_GLOBAL", gitconfig.to_str().unwrap()),
            ("KNIT_TEST_TOKEN", "scoped-secret"),
        ],
    );
    assert!(
        pull.contains("recovered") && pull.contains("private"),
        "the scoped recovery pull must clone the missing checkout: {pull}"
    );
    assert!(target.join("private").join(".git").exists());
    // The out-of-scope repos were neither cloned nor added.
    assert!(!target.join("frontend").exists());
    assert!(!target.join("backend").exists());
    let pending = read_json(&target.join(".knit/projects/demo.known-repos.json"));
    assert_eq!(
        pending["repos"],
        serde_json::json!({
            "backend": backend.to_string_lossy(),
            "frontend": frontend_url,
        })
    );

    fs::remove_dir_all(root).unwrap();
}

// The fixture drives a real PTY through the three guided credential flows:
// grouped clone prompts (same-host groups distinct), the no-metadata
// inferred fallback inside the clone, and pull recovery through the strict
// missing-assignment gate. Unix-only like the other PTY fixtures.
#[cfg(unix)]
#[test]
fn guided_clone_credentials_pty_grouped_inferred_and_pull_recovery() {
    let root = std::env::temp_dir().join(format!("knit-clone-guided-pty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/clone_guided_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_knit"))
        .arg(&root)
        // The fixture must never fall back to the test runner's cwd: `knit
        // auth` commands activate the invoking cwd's project.
        .current_dir(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    let _ = fs::remove_dir_all(&root);
    assert!(result.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("all conversations PASS"), "{stdout}");
}
