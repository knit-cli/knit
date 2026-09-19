mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::path::Path;

fn events(workspace: &Path) -> Vec<Value> {
    fs::read_to_string(workspace.join(".knit/history/demo.history.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn empty_bundle_creation_and_archive_record_history_at_write_time() {
    let root = unique_temp_dir();
    knit(&root, ["init", "demo"]);
    knit(&root, ["bundle", "empty work", "--offline"]);
    assert!(events(&root)
        .iter()
        .any(|event| event["kind"] == "bundle.created"));

    knit(&root, ["bundle", "archive", "empty-work"]);
    assert!(events(&root)
        .iter()
        .any(|event| event["kind"] == "bundle.archived"));
    let before = fs::read(root.join(".knit/history/demo.history.jsonl")).unwrap();
    let output = knit(
        &root,
        ["log", "--all", "--kind", "bundle.archived", "--json"],
    );
    let entries: Vec<Value> = serde_json::from_str(&output).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["bundleId"], "empty-work");
    assert_eq!(
        before,
        fs::read(root.join(".knit/history/demo.history.jsonl")).unwrap()
    );
}

#[test]
fn deletion_preserves_events_missing_from_an_older_ledger() {
    let root = unique_temp_dir();
    knit(&root, ["init", "demo"]);
    knit(&root, ["bundle", "legacy work", "--offline"]);
    fs::remove_file(root.join(".knit/history/demo.history.jsonl")).unwrap();
    knit(&root, ["bundle", "delete", "legacy-work", "--force"]);

    assert!(!root.join(".knit/bundles/legacy-work.bundle.json").exists());
    assert!(events(&root)
        .iter()
        .any(|event| event["bundleId"] == "legacy-work"));
    let output = knit(&root, ["log", "--all", "--json"]);
    let entries: Vec<Value> = serde_json::from_str(&output).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["bundleId"], "legacy-work");
}
