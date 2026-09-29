mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;

#[test]
fn generated_fork_plan_conforms_to_published_schema() {
    let root = unique_temp_dir();
    let mut bundle = serde_json::to_value(knit::model::ChangeGroup::new(
        "contribution".into(),
        "Synthetic contribution".into(),
        "2026-01-01T00:00:00Z".into(),
    ))
    .unwrap();
    bundle["repos"] = json!([{
        "id": "widget", "path": "/portable/widget", "baseBranch": "main",
        "baseSha": "a".repeat(40), "headSha": "b".repeat(40),
        "featureBranch": "feature",
        "remote": "https://github.com/contributor/widget.git",
        "sourceRemote": "https://github.com/contributor/widget.git",
        "targetRemote": "https://github.com/upstream/widget.git"
    }]);
    bundle["publications"] = json!([{
        "repoId": "widget", "provider": "github", "kind": "pull_request",
        "number": 7, "url": "https://github.com/upstream/widget/pull/7",
        "baseBranch": "main", "headBranch": "feature", "state": "OPEN",
        "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z"
    }]);
    fs::write(
        root.join("bundle.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
    let output = knit(
        &root,
        [
            "land",
            "plan",
            "--from-artifact",
            "bundle.json",
            "--out",
            "plan.json",
            "--json",
        ],
    );
    let mut plan: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        plan["repositoryIdentities"]["widget"]["targetRemote"],
        bundle["repos"][0]["targetRemote"]
    );
    let schema: Value =
        serde_json::from_str(include_str!("../schemas/land-plan.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(
        validator.is_valid(&plan),
        "{:?}",
        validator
            .iter_errors(&plan)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
    plan["repositoryIdentities"]["widget"]["targetRemote"] = json!(42);
    assert!(!validator.is_valid(&plan));
    fs::remove_dir_all(root).unwrap();
}
