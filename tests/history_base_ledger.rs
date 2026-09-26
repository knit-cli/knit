mod common;
use common::*;
use serde_json::Value;
use std::fs;

fn events(workspace: &std::path::Path, scope: &str) -> Vec<Value> {
    let output = knit(workspace, ["log", "--all", "--scope", scope, "--json"]);
    let groups: Vec<Value> = serde_json::from_str(&output).unwrap();
    groups
        .into_iter()
        .flat_map(|g| g["events"].as_array().unwrap().clone())
        .collect()
}

/// Every page of every grouping of every scope: obsolete rows must never
/// surface, no matter how the reader slices the ledger.
fn paged_events(workspace: &std::path::Path, scope: &str, group: &str, skip: usize) -> Vec<Value> {
    let output = knit(
        workspace,
        [
            "log",
            "--all",
            "--scope",
            scope,
            "--group",
            group,
            "--json",
            "--limit",
            "1",
            "--skip",
            &skip.to_string(),
        ],
    );
    let groups: Vec<Value> = serde_json::from_str(&output).unwrap();
    groups
        .into_iter()
        .flat_map(|g| g["events"].as_array().unwrap().clone())
        .collect()
}

fn ledger_text(workspace: &std::path::Path) -> String {
    fs::read_to_string(workspace.join(".knit/history/demo.history.jsonl")).unwrap()
}

fn ledger_events(workspace: &std::path::Path) -> Vec<Value> {
    ledger_text(workspace)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Record a synthetic intermediate-landing receipt on a bundle artifact, as a
/// real `knit land --target staging` run does. The bundle stays open.
fn add_staging_receipt(workspace: &std::path::Path, bundle_id: &str) {
    let path = workspace
        .join(".knit/bundles")
        .join(format!("{bundle_id}.bundle.json"));
    let mut bundle: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    bundle["nodes"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "id": "receipt-staging",
            "type": "branch.landed",
            "createdAt": "2026-09-20T10:00:00.000Z",
            "repoIds": ["backend"],
            "message": "Landed into staging",
            "runId": "run-staging",
            "landing": {"terminal": false, "targetBranch": "staging"}
        }));
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&bundle).unwrap()),
    )
    .unwrap();
}

/// Append synthetic rows to the recorded ledger by hand: obsolete raw-Git
/// rows of the kind older versions generated, plus a preserved row from a
/// bundle whose artifact is gone.
fn append_synthetic_rows(workspace: &std::path::Path) {
    let mut text = ledger_text(workspace);
    for sha in [
        "aaaa000000000000000000000000000000000001",
        "bbbb000000000000000000000000000000000002",
    ] {
        text.push_str(&format!(
            "{}\n",
            serde_json::json!({
                "schemaVersion": "knit.history.event.v1",
                "eventId": format!("khist_stale-{sha}"),
                "projectId": "demo",
                "kind": "base.commit",
                "repoId": "backend",
                "branch": "main",
                "baseBranch": "main",
                "commit": sha,
                "message": "Direct base change",
                "occurredAt": "2026-09-25T12:00:00.000Z",
                "recordedAt": "2026-09-25T12:00:00.000Z",
                "recordedBy": "knit"
            })
        ));
    }
    text.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "schemaVersion": "knit.history.event.v1",
            "eventId": "khist_ghost0000000001",
            "projectId": "demo",
            "kind": "commit.recorded",
            "bundleId": "ghost-work",
            "bundleTitle": "ghost work",
            "repoId": "backend",
            "nodeId": "ghost-node",
            "commit": "cccc000000000000000000000000000000000003",
            "message": "Deleted bundle work",
            "occurredAt": "2026-09-24T12:00:00.000Z",
            "recordedAt": "2026-09-24T12:00:00.000Z",
            "recordedBy": "knit"
        })
    ));
    fs::write(workspace.join(".knit/history/demo.history.jsonl"), text).unwrap();
}

#[test]
fn direct_git_history_stays_out_and_scopes_follow_bundle_lifecycle() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    let backend = root.join("backend");
    append_line(&backend.join("app.txt"), "direct base change");
    git(&backend, ["commit", "-am", "Direct base change"]);
    let base_sha = git(&backend, ["rev-parse", "HEAD"]).trim().to_string();
    knit(
        &workspace,
        ["bundle", "ongoing change", "--repo", "backend"],
    );
    let checkout = workspace.join(".knit/worktrees/ongoing-change/backend");
    append_line(&checkout.join("app.txt"), "ongoing change");
    knit(&checkout, ["commit", "--all", "-m", "Ongoing change"]);
    let ongoing_sha = git(&checkout, ["rev-parse", "HEAD"]).trim().to_string();
    // An intermediate landing records a receipt; the bundle stays open.
    add_staging_receipt(&workspace, "ongoing-change");
    knit(&workspace, ["history", "refresh"]);

    // Direct Git commits on a configured base are never imported: history
    // exists only inside bundle context. The direct sha may appear as a
    // bundle's fork point (`beforeSha`), never as a recorded commit row.
    let ledger = ledger_events(&workspace);
    assert!(
        !ledger.iter().any(|e| e["kind"] == "base.commit"),
        "{ledger:?}"
    );
    assert!(
        !ledger.iter().any(|e| e["commit"] == base_sha.as_str()),
        "{ledger:?}"
    );

    // Only an open bundle exists, so the completed reading is empty and the
    // open reading carries the authoring commit plus the staging receipt.
    assert!(events(&workspace, "base").is_empty());
    let ongoing = events(&workspace, "ongoing");
    assert!(ongoing.iter().any(|e| e["commit"] == ongoing_sha));
    assert!(ongoing
        .iter()
        .any(|e| e["kind"] == "branch.landed" && e["branch"] == "staging"));
    let both = events(&workspace, "base-and-ongoing");
    assert!(both.iter().any(|e| e["commit"] == ongoing_sha));

    // The project log defaults to full activity, matching the explicit scope.
    let default_history = knit(&workspace, ["log", "--all", "--json"]);
    let explicit_activity = knit(
        &workspace,
        ["log", "--all", "--scope", "activity", "--json"],
    );
    assert_eq!(default_history, explicit_activity);
    assert!(default_history.contains(&ongoing_sha));

    // The bundle log keeps its activity reading.
    assert!(knit(&checkout, ["log", "--json"]).contains(&ongoing_sha));

    let first = ledger_text(&workspace);
    knit(&workspace, ["history", "refresh"]);
    assert_eq!(first, ledger_text(&workspace), "refresh must be idempotent");

    // Archiving completes the bundle: its full original activity — commits
    // and receipt included — moves to the base reading, shown as recorded.
    knit(
        &workspace,
        ["bundle", "archive", "ongoing-change", "--keep-worktrees"],
    );
    knit(&workspace, ["history", "refresh"]);
    let base = events(&workspace, "base");
    assert!(base.iter().any(|e| e["commit"] == ongoing_sha));
    assert!(base.iter().any(|e| e["kind"] == "bundle.archived"));
    assert!(base
        .iter()
        .any(|e| e["kind"] == "branch.landed" && e["branch"] == "staging"));
    assert!(events(&workspace, "ongoing").is_empty());
    assert!(events(&workspace, "activity")
        .iter()
        .any(|e| e["commit"] == ongoing_sha));
    assert!(!ledger_events(&workspace)
        .iter()
        .any(|e| e["commit"] == base_sha.as_str()));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stale_base_commit_rows_are_hidden_from_every_page_and_rebuild_cleans_them() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    knit(&workspace, ["bundle", "live work", "--repo", "backend"]);
    let checkout = workspace.join(".knit/worktrees/live-work/backend");
    append_line(&checkout.join("app.txt"), "live change");
    knit(&checkout, ["commit", "--all", "-m", "Live change"]);
    let live_sha = git(&checkout, ["rev-parse", "HEAD"]).trim().to_string();
    knit(&workspace, ["history", "refresh"]);
    append_synthetic_rows(&workspace);

    for scope in [
        "activity",
        "base",
        "ongoing",
        "base-and-ongoing",
        "landings",
    ] {
        for group in ["event", "commit", "bundle"] {
            for skip in 0..4 {
                for event in paged_events(&workspace, scope, group, skip) {
                    assert_ne!(event["kind"], "base.commit", "scope {scope} group {group}");
                }
            }
        }
    }
    let activity = events(&workspace, "activity");
    assert!(activity.iter().any(|e| e["commit"] == live_sha));
    assert!(activity
        .iter()
        .any(|e| e["bundleId"] == "ghost-work" && e["message"] == "Deleted bundle work"));
    let display = knit(&workspace, ["log", "--all", "--oneline"]);
    assert!(!display.contains("[base]"));

    // Refresh is append-only: it neither trusts nor repeats the stale rows.
    knit(&workspace, ["history", "refresh"]);
    assert!(ledger_text(&workspace).contains("base.commit"));

    // Rebuild removes the obsolete generated rows and keeps real history.
    knit(&workspace, ["history", "refresh", "--rebuild"]);
    let rebuilt = ledger_text(&workspace);
    assert!(!rebuilt.contains("base.commit"), "{rebuilt}");
    assert!(rebuilt.contains("Deleted bundle work"));
    assert!(rebuilt.contains(&live_sha));
    let after = events(&workspace, "activity");
    assert!(after.iter().any(|e| e["commit"] == live_sha));
    assert!(after
        .iter()
        .any(|e| e["bundleId"] == "ghost-work" && e["message"] == "Deleted bundle work"));
    fs::remove_dir_all(root).unwrap();
}
