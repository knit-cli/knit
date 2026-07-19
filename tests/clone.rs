mod common;

use common::*;
use std::fs;
use std::path::Path;

fn bundle_payload(id: &str, repo_ids: &[&str]) -> serde_json::Value {
    let repos: Vec<serde_json::Value> = repo_ids
        .iter()
        .map(|repo_id| {
            serde_json::json!({
                "id": repo_id,
                "path": format!("/tmp/{repo_id}"),
                "baseBranch": "main",
            })
        })
        .collect();
    serde_json::json!({
        "schemaVersion": "1",
        "kind": "knit.bundle",
        "id": id,
        "title": id,
        "createdAt": "2026-01-01T00:00:00Z",
        "updatedAt": "2026-01-01T00:00:00Z",
        "repos": repos,
        "commitGroups": [],
    })
}

/// An export with one cloneable repo (`backend`), one repo whose clone URL is
/// broken (`frontend`), one withheld private repo (count only), one restorable
/// bundle, and one bundle that references the failed repo.
fn partial_export(root: &Path) -> (serde_json::Value, std::path::PathBuf) {
    let source = root.join("backend-source");
    init_repo(&source, "backend");
    let export = serde_json::json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [
                {
                    "localId": "backend",
                    "name": "backend",
                    "defaultBranch": null,
                    "remoteUrl": source.to_string_lossy(),
                    "metadata": {},
                },
                {
                    "localId": "frontend",
                    "name": "frontend",
                    "defaultBranch": null,
                    "remoteUrl": root.join("no-such-repo").to_string_lossy(),
                    "metadata": {},
                },
            ],
            "omittedRepositoryCount": 1,
            "bundles": [
                {
                    "id": "rb-1",
                    "slug": "feature-a",
                    "lifecycleState": "open",
                    "currentArtifact": {
                        "artifactHash": "hash-a",
                        "payload": bundle_payload("feature-a", &["backend"]),
                    },
                },
                {
                    "id": "rb-2",
                    "slug": "feature-c",
                    "lifecycleState": "open",
                    "currentArtifact": {
                        "artifactHash": "hash-c",
                        "payload": bundle_payload("feature-c", &["backend", "frontend"]),
                    },
                },
            ],
            "historyEvents": [],
        }
    });
    (export, source)
}

#[test]
fn clone_json_reports_repos_bundles_and_dropped_bundles() {
    let root = unique_temp_dir();
    let (export, _source) = partial_export(&root);
    let base_url = spawn_fake_knithub_with_body(export.to_string());
    let target = root.join("workspace");

    let (stdout, stderr, success) = knit_split_output(
        &root,
        &[
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
            "--json",
        ],
        &[],
    );

    // Partial success still exits 0: the project was created.
    assert!(success, "clone --json should succeed: {stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout must be pure JSON ({error}): {stdout}"));

    assert_eq!(document["project"]["id"], "demo");
    assert_eq!(document["project"]["owner"], "acme");
    assert_eq!(document["project"]["slug"], "demo");
    assert_eq!(
        document["targetPath"].as_str().unwrap(),
        target.to_string_lossy()
    );
    assert_eq!(
        document["repos"],
        serde_json::json!([
            {"id": "backend", "status": "cloned"},
            {
                "id": "frontend",
                "status": "failed",
                "error": document["repos"][1]["error"],
            },
        ])
    );
    assert!(document["repos"][1]["error"]
        .as_str()
        .unwrap()
        .contains("frontend"));
    assert_eq!(document["clonedRepoCount"], 1);
    assert_eq!(document["failedRepoCount"], 1);
    assert_eq!(document["omittedRepositoryCount"], 1);
    assert_eq!(
        document["bundles"]["restored"],
        serde_json::json!(["feature-a"])
    );
    assert_eq!(
        document["bundles"]["dropped"],
        serde_json::json!([{"id": "feature-c", "missingRepos": ["frontend"]}])
    );
    assert_eq!(document["activeBundle"], "feature-a");
    assert_eq!(document["worktreesMaterialized"], false);

    // Human progress moved to stderr, including the dropped-bundle mention.
    assert!(stderr.contains("dropped bundle"), "stderr: {stderr}");
    assert!(target.join(".knit/bundles/feature-a.bundle.json").exists());
    assert!(!target.join(".knit/bundles/feature-c.bundle.json").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn clone_human_output_mentions_dropped_bundles() {
    let root = unique_temp_dir();
    let (export, _source) = partial_export(&root);
    let base_url = spawn_fake_knithub_with_body(export.to_string());
    let target = root.join("workspace");

    let output = knit(
        &root,
        [
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
        ],
    );

    assert!(
        output.contains("dropped bundle") && output.contains("feature-c"),
        "human output must mention dropped bundles: {output}"
    );
    assert!(output.contains("frontend"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_projects_json_lists_projects_outside_any_workspace() {
    let root = unique_temp_dir();
    let outside = root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let body = serde_json::json!({
        "data": [
            {
                "id": "p-1",
                "name": "Knit Tools",
                "slug": "knit-tools",
                "description": "workspace",
                "visibility": "private",
                "owner": {"type": "user", "id": "u-1", "name": "Marc", "slug": "marc-merino"},
                "organization": null,
            },
            {
                "id": "p-2",
                "name": "Acme App",
                "slug": "acme-app",
                "description": null,
                "visibility": "public",
                "owner": {"type": "organization", "id": "o-1", "name": "Acme", "slug": "acme"},
                "organization": {"id": "o-1", "name": "Acme", "slug": "acme"},
            },
        ]
    });
    let base_url = spawn_fake_knithub_with_body(body.to_string());
    // A per-test global config proves the verb works outside any workspace.
    let knit_home = root.join("knit-home");
    fs::create_dir_all(&knit_home).unwrap();
    let env = [
        ("KNIT_HOME", knit_home.to_str().unwrap()),
        ("KNIT_REMOTE_TOKEN", "test-token"),
    ];
    let add = knit_with_env(
        &outside,
        ["remote", "add", "hosted", &base_url, "--global"],
        &env,
    );
    assert!(add.contains("hosted"));

    let (stdout, _stderr, success) =
        knit_split_output(&outside, &["remote", "projects", "--json"], &env);
    assert!(success);
    let document: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout must be pure JSON ({error}): {stdout}"));
    assert_eq!(document["remote"], "hosted");
    assert_eq!(document["url"], base_url);
    assert_eq!(
        document["projects"],
        serde_json::json!([
            {
                "id": "p-1",
                "owner": "marc-merino",
                "slug": "knit-tools",
                "name": "Knit Tools",
                "description": "workspace",
                "visibility": "private",
            },
            {
                "id": "p-2",
                "owner": "acme",
                "slug": "acme-app",
                "name": "Acme App",
                "description": null,
                "visibility": "public",
            },
        ])
    );

    // Human mode shows an owner/slug table on stdout.
    let human = knit_with_env(&outside, ["remote", "projects"], &env);
    assert!(human.contains("marc-merino/knit-tools"));
    assert!(human.contains("acme/acme-app"));
    assert!(human.contains("private"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_projects_json_error_envelopes() {
    let root = unique_temp_dir();
    let outside = root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let knit_home = root.join("knit-home");
    fs::create_dir_all(&knit_home).unwrap();
    let home_env = ("KNIT_HOME", knit_home.to_str().unwrap());

    // No remote configured at all.
    let (stdout, _stderr, success) =
        knit_split_output(&outside, &["remote", "projects", "--json"], &[home_env]);
    assert!(!success);
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(envelope["error"]["kind"], "noRemote");
    assert!(!envelope["error"]["message"].as_str().unwrap().is_empty());

    // Remote configured but no token anywhere.
    let (stdout, _stderr, success) = knit_split_output(
        &outside,
        &["remote", "projects", "--remote", "hosted", "--json"],
        &[home_env, ("KNIT_REMOTE_URL", "http://127.0.0.1:1")],
    );
    assert!(!success);
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(envelope["error"]["kind"], "noToken");

    // Remote and token resolve, but the endpoint refuses connections.
    let unreachable = unreachable_remote_url();
    let (stdout, _stderr, success) = knit_split_output(
        &outside,
        &["remote", "projects", "--remote", "hosted", "--json"],
        &[
            home_env,
            ("KNIT_REMOTE_URL", unreachable.as_str()),
            ("KNIT_REMOTE_TOKEN", "test-token"),
        ],
    );
    assert!(!success);
    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(envelope["error"]["kind"], "http");

    fs::remove_dir_all(root).unwrap();
}

/// End to end vended-credential clone: the repo is served over dumb HTTP
/// behind basic auth, so the plain clone fails, knit vends the credential from
/// the fake server, and the askpass retry clones successfully.
#[test]
fn clone_retries_private_repo_with_vended_credential() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake");
    fs::create_dir_all(&fake_dir).unwrap();

    // A real repo, exported as a bare dumb-HTTP tree under the fake server.
    let work = root.join("work");
    init_repo(&work, "backend");
    let served = fake_dir.join("git/backend.git");
    fs::create_dir_all(served.parent().unwrap()).unwrap();
    git(
        &root,
        [
            "clone",
            "--bare",
            work.to_str().unwrap(),
            served.to_str().unwrap(),
        ],
    );
    git(&served, ["update-server-info"]);

    let base_url = spawn_fake_remote_api(&fake_dir, String::new());
    let export = serde_json::json!({
        "data": {
            "project": {"id": "p-1", "slug": "demo"},
            "knitProject": null,
            "repositories": [{
                "id": "r-1",
                "localId": "backend",
                "name": "backend",
                "defaultBranch": "main",
                "remoteUrl": format!("{base_url}/git/backend.git"),
                "visibility": "private",
                "metadata": {},
            }],
            "omittedRepositoryCount": 0,
            "bundles": [],
            "historyEvents": [],
        }
    });
    fs::write(fake_dir.join("export.json"), export.to_string()).unwrap();

    let target = root.join("workspace");
    let git_home = root.join("git-home");
    fs::create_dir_all(&git_home).unwrap();
    let (stdout, stderr, success) = knit_split_output(
        &root,
        &[
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
            "--json",
        ],
        &[
            // Neutralize ambient git credential helpers/prompts so the plain
            // attempt fails deterministically and the retry uses only askpass.
            ("HOME", git_home.to_str().unwrap()),
            ("XDG_CONFIG_HOME", git_home.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
        ],
    );

    assert!(success, "vended clone should succeed: {stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout must be pure JSON ({error}): {stdout}"));
    assert_eq!(
        document["repos"],
        serde_json::json!([
            {"id": "backend", "status": "cloned", "credential": "remote-vended"},
        ])
    );
    assert!(target.join("backend/.git").exists());

    // The vend endpoint was called with the export's server ids and the API token.
    let vends = fs::read_to_string(fake_dir.join("vend-requests.txt")).unwrap();
    assert!(
        vends.contains("/api/v1/projects/p-1/repositories/r-1/git-credential Bearer test-token"),
        "vend request should carry ids and bearer token: {vends}"
    );

    // The credential never reached disk: no askpass shim or credential text
    // remains anywhere under the workspace.
    let leaked = walk_contains(&target, "vended-pass");
    assert!(leaked.is_empty(), "credential leaked into: {leaked:?}");

    fs::remove_dir_all(root).unwrap();
}

/// A vend answered with 409 noCredential keeps the repo failed and appends the
/// contract hint to the error, in both the human and JSON outputs.
#[test]
fn clone_vend_409_keeps_repo_failed_with_hint() {
    let root = unique_temp_dir();
    let fake_dir = root.join("fake");
    fs::create_dir_all(&fake_dir).unwrap();
    fs::write(fake_dir.join("vend-409"), "").unwrap();

    let base_url = spawn_fake_remote_api(&fake_dir, String::new());
    let export = serde_json::json!({
        "data": {
            "project": {"id": "p-1", "slug": "demo"},
            "knitProject": null,
            "repositories": [
                {
                    "id": "r-1",
                    "localId": "backend",
                    "name": "backend",
                    "defaultBranch": null,
                    "remoteUrl": root.join("no-such-repo").to_string_lossy(),
                    "visibility": "private",
                    "metadata": {},
                },
                {
                    "id": "r-2",
                    "localId": "docs",
                    "name": "docs",
                    "defaultBranch": null,
                    "remoteUrl": docs_source(&root).to_string_lossy(),
                    "visibility": "public",
                    "metadata": {},
                },
            ],
            "omittedRepositoryCount": 0,
            "bundles": [],
            "historyEvents": [],
        }
    });
    fs::write(fake_dir.join("export.json"), export.to_string()).unwrap();

    let target = root.join("workspace");
    let (stdout, stderr, success) = knit_split_output(
        &root,
        &[
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "test-token",
            "--no-worktree",
            "--json",
        ],
        &[],
    );

    assert!(success, "partial clone should still succeed: {stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(document["repos"][0]["id"], "backend");
    assert_eq!(document["repos"][0]["status"], "failed");
    let error = document["repos"][0]["error"].as_str().unwrap();
    assert!(
        error.contains("no git access: configure SSH keys or ask a project member to connect GitHub on the sync remote"),
        "409 vend must append the hint: {error}"
    );
    assert!(stderr.contains("no git access"), "hint in human output too");

    fs::remove_dir_all(root).unwrap();
}

fn docs_source(root: &Path) -> std::path::PathBuf {
    let source = root.join("docs-source");
    if !source.exists() {
        init_repo(&source, "docs");
    }
    source
}

/// Recursively list files under `root` whose content contains `needle`.
fn walk_contains(root: &Path, needle: &str) -> Vec<std::path::PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                if String::from_utf8_lossy(&bytes).contains(needle) {
                    hits.push(path);
                }
            }
        }
    }
    hits
}

#[test]
fn clone_survives_history_events_missing_project_id() {
    let root = unique_temp_dir();
    let (mut export, _source) = partial_export(&root);
    // A native server-side event without projectId (real production shape)
    // plus an unreadable one: the clone must succeed, repair the former, and
    // skip the latter with a warning.
    export["data"]["historyEvents"] = serde_json::json!([
        {
            "schemaVersion": "knit.history.event.v1",
            "eventId": "review-decision:prod-shape",
            "kind": "review.approved",
            "bundleId": "feature-a",
            "recordedAt": "2026-07-18T15:53:42Z",
            "recordedBy": "native",
        },
        {"eventId": 42},
    ]);
    let base_url = spawn_fake_knithub_with_body(export.to_string());
    let target = root.join("workspace");

    let (_stdout, stderr, success) = knit_split_output(
        &root,
        &[
            "clone",
            "acme/demo",
            target.to_str().unwrap(),
            "--remote",
            "hosted",
            "--url",
            &base_url,
            "--token",
            "secret-token",
            "--no-worktree",
        ],
        &[],
    );

    assert!(success, "{stderr}");
    // Without --json, human lines (including the skip warning) stay on stdout.
    assert!(
        _stdout.contains("skipped 1 unreadable remote history event"),
        "stdout: {_stdout}\nstderr: {stderr}"
    );
    let history = std::fs::read_to_string(target.join(".knit/history/demo.history.jsonl")).unwrap();
    assert!(history.contains("review-decision:prod-shape"), "{history}");
    assert!(history.contains("\"projectId\":\"demo\""), "{history}");

    fs::remove_dir_all(root).unwrap();
}
