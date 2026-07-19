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
