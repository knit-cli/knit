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
