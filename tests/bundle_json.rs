mod common;

use common::*;
use serde_json::Value;
use std::fs;

#[test]
fn bundle_create_json_keeps_stdout_machine_readable() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    let (stdout, stderr, success) = knit_split_output(
        &workspace,
        &[
            "bundle",
            "JSON feature",
            "--project",
            "demo",
            "--repo",
            "backend",
            "--json",
        ],
        &[],
    );

    assert!(success, "bundle create failed: {stderr}");
    assert_eq!(stdout.lines().count(), 1, "stdout was not one JSON line");
    let document: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout must be pure JSON ({error}): {stdout}"));
    assert_eq!(
        document,
        serde_json::json!({
            "bundleId": "json-feature",
            "bundleRoot": workspace
                .join(".knit/worktrees/json-feature")
                .to_string_lossy(),
        })
    );
    assert!(stderr.contains("backend: base"), "stderr: {stderr}");
    assert!(stderr.contains("added backend"), "stderr: {stderr}");
    assert!(stderr.contains("Active bundle:"), "stderr: {stderr}");
    assert!(workspace
        .join(".knit/worktrees/json-feature/backend")
        .join(".git")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_create_json_uses_the_standard_error_envelope() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let (_stdout, stderr, success) =
        knit_split_output(&workspace, &["bundle", "Duplicate", "--json"], &[]);
    assert!(success, "first bundle create failed: {stderr}");

    let (stdout, stderr, success) =
        knit_split_output(&workspace, &["bundle", "Duplicate", "--json"], &[]);
    assert!(!success, "duplicate bundle create unexpectedly succeeded");
    assert_eq!(stdout.lines().count(), 1, "stdout was not one JSON line");
    let envelope: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout must be pure JSON ({error}): {stdout}"));
    assert_eq!(envelope["error"]["kind"], "other");
    assert!(envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already exists"));
    assert!(stderr.contains("already exists"), "stderr: {stderr}");

    fs::remove_dir_all(root).unwrap();
}
