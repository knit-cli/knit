//! The machine-readable documents a Knit host reads (`docs/harness.md`).
//! These shapes are a contract with external drivers; the assertions here are
//! what makes a silent rename fail the build.

mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::path::Path;

#[test]
fn status_json_documents_the_bundle_and_every_repo() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    let document: Value =
        serde_json::from_str(&knit(&workspace, ["status", "--json"])).expect("status --json");

    assert_eq!(document["bundle"], "venue-capacity");
    assert_eq!(document["state"], "open");
    assert!(
        document["resolvedFrom"].as_str().is_some(),
        "resolvedFrom must say how the bundle was resolved"
    );

    let repos = document["repos"].as_array().expect("repos array");
    assert_eq!(repos.len(), 1);
    let repo = &repos[0];
    assert_eq!(repo["id"], "backend");
    assert_eq!(repo["expectedBranch"], "knit/venue-capacity");
    assert_eq!(repo["branch"], "knit/venue-capacity");
    assert_eq!(repo["checkoutPresent"], true);
    assert_eq!(repo["wrongBranch"], false);
    assert_eq!(repo["mode"], "worktree");
    assert_eq!(repo["status"], "clean");

    assert_eq!(document["publications"]["repos"], 1);
    assert_eq!(document["publications"]["reviews"], 0);
}

#[test]
fn status_json_reports_a_missing_worktree_without_a_branch() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    fs::remove_dir_all(workspace.join(".knit/worktrees/venue-capacity/backend")).unwrap();

    let document: Value =
        serde_json::from_str(&knit(&workspace, ["status", "--json"])).expect("status --json");
    let repo = &document["repos"][0];

    assert_eq!(repo["checkoutPresent"], false);
    assert_eq!(repo["branch"], Value::Null);
    assert_eq!(repo["status"], "missing worktree");
}

#[test]
fn bundle_list_json_marks_the_active_bundle_and_points_at_each_artifact() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    knit(&workspace, ["bundle", "ticket refunds"]);

    let document: Value = serde_json::from_str(&knit(&workspace, ["bundle", "list", "--json"]))
        .expect("bundle list --json");

    assert_eq!(document["activeBundle"], "ticket-refunds");
    let bundles = document["bundles"].as_array().expect("bundles array");
    assert_eq!(bundles.len(), 2);

    let venue = bundles
        .iter()
        .find(|bundle| bundle["id"] == "venue-capacity")
        .expect("venue-capacity listed");
    assert_eq!(venue["state"], "open");
    assert_eq!(venue["active"], false);
    assert_eq!(venue["title"], "venue capacity");
    assert_eq!(venue["repos"].as_array().unwrap().len(), 1);
    assert!(
        Path::new(venue["path"].as_str().expect("artifact path")).is_file(),
        "the listed artifact path must be readable by the host"
    );

    let refunds = bundles
        .iter()
        .find(|bundle| bundle["id"] == "ticket-refunds")
        .expect("ticket-refunds listed");
    assert_eq!(refunds["active"], true);
}

#[test]
fn bundle_list_json_hides_archived_bundles_unless_asked() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    knit(&workspace, ["bundle", "archive", "venue-capacity"]);

    let hidden: Value = serde_json::from_str(&knit(&workspace, ["bundle", "list", "--json"]))
        .expect("bundle list --json");
    assert!(hidden["bundles"].as_array().unwrap().is_empty());

    let shown: Value = serde_json::from_str(&knit(
        &workspace,
        ["bundle", "list", "--archived", "--json"],
    ))
    .expect("bundle list --archived --json");
    let bundles = shown["bundles"].as_array().unwrap();
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0]["state"], "archived");
}

/// Every `--json` command fails the same way (`docs/harness.md`): one JSON
/// document `{"error":{"kind","message"}}` on stdout, the human `Error:` line
/// on stderr, and a non-zero exit.
#[track_caller]
fn assert_json_failure(cwd: &Path, args: &[&str], kind: &str) -> Value {
    let (stdout, stderr, success) = knit_split_output(cwd, args, &[]);
    assert!(!success, "knit {args:?} should fail");
    let envelope: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("knit {args:?} stdout is not one JSON document ({error}): {stdout:?}")
    });
    assert_eq!(envelope["error"]["kind"], kind, "knit {args:?}: {envelope}");
    let message = envelope["error"]["message"].as_str().expect("message");
    assert!(!message.is_empty());
    assert!(
        stderr.contains("rror"),
        "knit {args:?} must still explain the failure on stderr: {stderr:?}"
    );
    let schema: Value =
        serde_json::from_str(&knit(cwd, ["schema", "print", "error"])).expect("error schema");
    let validator = jsonschema::validator_for(&schema).expect("error schema compiles");
    assert!(
        validator.is_valid(&envelope),
        "knit {args:?} envelope does not match schemas/error.schema.json: {envelope}"
    );
    envelope
}

#[test]
fn status_and_bundle_list_json_fail_with_the_error_envelope_outside_a_workspace() {
    // Both used to print nothing on stdout.
    let outside = unique_temp_dir();
    assert_json_failure(&outside, &["status", "--json"], "noWorkspace");
    assert_json_failure(&outside, &["bundle", "list", "--json"], "noWorkspace");
}

#[test]
fn status_json_reports_no_bundle_when_none_resolves() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    knit(&workspace, ["bundle", "ticket refunds"]);
    // No workspace fallback: from the workspace root, nothing resolves.
    let config_path = workspace.join(".knit/config.json");
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("activeBundle");
    fs::write(&config_path, config.to_string()).unwrap();

    let envelope = assert_json_failure(&workspace, &["status", "--json"], "noBundle");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(message.contains("venue-capacity"), "{message}");
}

#[test]
fn remote_auth_status_json_reports_a_missing_remote_as_json() {
    // Used to print only plain text.
    let workspace = unique_temp_dir();
    assert_json_failure(
        &workspace,
        &["remote", "auth-status", "hosted", "--json"],
        "noRemote",
    );
}

#[test]
fn bundle_pull_json_reports_a_missing_sync_remote_as_no_remote() {
    // Used to report `http`.
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(&workspace, ["bundle", "venue capacity"]);

    let envelope = assert_json_failure(
        &workspace,
        &["bundle", "pull", "venue-capacity", "--json"],
        "noRemote",
    );
    assert!(envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("No sync remote configured"));
}

#[test]
fn a_rejected_json_command_line_reports_usage() {
    let workspace = unique_temp_dir();
    let envelope = assert_json_failure(&workspace, &["status", "--json", "--bogus"], "usage");
    assert!(envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("--bogus"));
    // Without --json, clap's own output is untouched.
    let (stdout, _, success) = knit_split_output(&workspace, &["status", "--bogus"], &[]);
    assert!(!success);
    assert!(stdout.is_empty(), "{stdout:?}");
}

#[test]
fn a_failure_that_already_printed_its_report_adds_no_envelope() {
    // `land validate` answers with its validation report even when the plan
    // is invalid; that report is the one document on stdout.
    let workspace = unique_temp_dir();
    let plan = workspace.join("plan.json");
    fs::write(&plan, "{}").unwrap();
    let (stdout, _, success) = knit_split_output(
        &workspace,
        &[
            "land",
            "validate",
            "--plan",
            plan.to_str().unwrap(),
            "--json",
        ],
        &[],
    );
    assert!(!success);
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("one JSON document expected ({error}): {stdout:?}"));
    assert_eq!(report["valid"], false);
    assert!(report.get("error").is_none(), "{report}");
}
