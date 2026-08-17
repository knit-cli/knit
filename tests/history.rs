mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn history_path(workspace: &Path) -> PathBuf {
    workspace.join(".knit/history/demo.history.jsonl")
}

fn history_events(workspace: &Path) -> Vec<Value> {
    let text = fs::read_to_string(history_path(workspace)).unwrap();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn event_of_kind<'a>(events: &'a [Value], kind: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["kind"].as_str() == Some(kind))
        .unwrap_or_else(|| panic!("no {kind} event in {events:#?}"))
}

fn bundle_artifact(workspace: &Path, slug: &str) -> Value {
    let path = workspace
        .join(".knit/bundles")
        .join(format!("{slug}.bundle.json"));
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn write_bundle_artifact(workspace: &Path, slug: &str, artifact: &Value) {
    let path = workspace
        .join(".knit/bundles")
        .join(format!("{slug}.bundle.json"));
    fs::write(path, serde_json::to_string_pretty(artifact).unwrap()).unwrap();
}

/// A bundle with one Knit commit and one commit made outside Knit, dated well
/// before the sweep that records it.
fn workspace_with_recorded_and_observed_commits(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, root);
    knit(
        &workspace,
        ["bundle", "venue capacity", "--repo", "backend"],
    );

    let checkout = workspace.join(".knit/worktrees/venue-capacity/backend");
    append_line(&checkout.join("app.txt"), "capacity form");
    knit(
        &workspace,
        [
            "commit",
            "--all",
            "-m",
            "Add capacity form\n\nBody paragraph nobody wants in a listing.",
        ],
    );

    append_line(&checkout.join("app.txt"), "outside knit");
    git(&checkout, ["add", "app.txt"]);
    git(
        &checkout,
        [
            "commit",
            "--date",
            "2026-03-04T09:15:00+00:00",
            "-m",
            "Fix seat map rounding",
        ],
    );
    knit(&workspace, ["sync"]);

    workspace
}

#[test]
fn observed_commits_are_named_and_timed_by_the_commit_itself() {
    let root = unique_temp_dir();
    let workspace = workspace_with_recorded_and_observed_commits(&root);

    // The observe path captures each commit's subject and author date, so the
    // ledger no longer holds bare SHAs.
    let artifact = bundle_artifact(&workspace, "venue-capacity");
    let observed = artifact["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["type"].as_str() == Some("git.observed"))
        .expect("observed node");
    let details = &observed["repoChanges"][0]["commitDetails"];
    let (sha, detail) = details.as_object().unwrap().iter().next().unwrap();
    assert_eq!(sha.len(), 40, "{details}");
    assert_eq!(
        detail["subject"].as_str(),
        Some("Fix seat map rounding"),
        "{details}"
    );
    assert!(
        detail["authoredAt"]
            .as_str()
            .unwrap()
            .starts_with("2026-03-04"),
        "{details}"
    );

    let events = history_events(&workspace);
    let observed = event_of_kind(&events, "commit.observed");
    assert_eq!(
        observed["message"].as_str(),
        Some("Fix seat map rounding"),
        "{observed}"
    );
    // The sweep time must not stand in for when the work happened.
    assert!(
        observed["occurredAt"]
            .as_str()
            .unwrap()
            .starts_with("2026-03-04"),
        "{observed}"
    );

    let listing = knit(&workspace, ["history", "list", "-n", "50"]);
    assert!(listing.contains("Fix seat map rounding"), "{listing}");
    // The kind string is a fallback, not a message.
    assert!(!listing.contains("commit.observed"), "{listing}");
    // One event is one line: commit bodies stay out of the listing.
    assert!(listing.contains("Add capacity form"), "{listing}");
    assert!(
        !listing.contains("Body paragraph nobody wants in a listing."),
        "{listing}"
    );
}

#[test]
fn lifecycle_nodes_reach_history_and_filters_narrow_the_listing() {
    let root = unique_temp_dir();
    let workspace = workspace_with_recorded_and_observed_commits(&root);
    knit(&workspace, ["bundle", "archive", "venue-capacity"]);
    knit(&workspace, ["bundle", "other work", "--repo", "frontend"]);

    let created = knit(&workspace, ["history", "list", "--kind", "bundle.created"]);
    assert!(created.contains("venue capacity"), "{created}");
    assert!(created.contains("other work"), "{created}");
    // Lifecycle events have no repo or commit of their own.
    assert!(created.contains("-        "), "{created}");

    let archived = knit(&workspace, ["history", "list", "--kind", "bundle.archived"]);
    assert!(archived.contains("venue-capacity"), "{archived}");

    let events = history_events(&workspace);
    let repo_added = event_of_kind(&events, "repo.added");
    assert!(repo_added["repoId"].as_str().is_some(), "{repo_added}");
    assert!(repo_added["commit"].is_null(), "{repo_added}");
    // Materializing a worktree is not history.
    assert!(
        events
            .iter()
            .all(|event| event["nodeType"].as_str() != Some("worktree.materialized")),
        "{events:#?}"
    );

    // Repeated --kind is a union.
    let both = knit(
        &workspace,
        [
            "history",
            "list",
            "-n",
            "50",
            "--kind",
            "bundle.created",
            "--kind",
            "commit.observed",
        ],
    );
    assert!(both.contains("Fix seat map rounding"), "{both}");
    assert!(both.contains("venue capacity"), "{both}");
    assert!(!both.contains("Add capacity form"), "{both}");

    // The parent command's bundle context stands in for the list filter.
    let scoped = knit(
        &workspace,
        ["--bundle", "venue-capacity", "history", "list", "-n", "50"],
    );
    assert!(scoped.contains("Fix seat map rounding"), "{scoped}");
    assert!(!scoped.contains("other work"), "{scoped}");
}

#[test]
fn rebuild_backfills_recorded_events_and_keeps_orphaned_ones() {
    let root = unique_temp_dir();
    let workspace = workspace_with_recorded_and_observed_commits(&root);

    // A ledger as it was recorded before commits carried any detail: no
    // message, and every event stamped with the sweep time.
    let mut events = history_events(&workspace);
    for event in &mut events {
        event.as_object_mut().unwrap().remove("message");
        event["occurredAt"] = Value::String("2026-08-14T09:00:00Z".to_string());
    }
    let mut ghost = events[0].clone();
    ghost["eventId"] = Value::String("khist_ghostevent0001".to_string());
    ghost["bundleId"] = Value::String("deleted-bundle".to_string());
    ghost["message"] = Value::String("Work from a deleted bundle".to_string());
    events.push(ghost);
    let recorded = events.len();
    fs::write(
        history_path(&workspace),
        events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();

    // Plain refresh is append-only: it has nothing to add and leaves the
    // recorded events exactly as they are.
    let refreshed = knit(&workspace, ["history", "refresh"]);
    assert!(refreshed.contains("0 new event(s)"), "{refreshed}");
    let after_refresh = history_events(&workspace);
    assert_eq!(after_refresh.len(), recorded);
    assert!(event_of_kind(&after_refresh, "commit.observed")["message"].is_null());

    let rebuilt = knit(&workspace, ["history", "refresh", "--rebuild"]);
    assert!(rebuilt.contains("rebuilt"), "{rebuilt}");
    assert!(rebuilt.contains("1 preserved event(s)"), "{rebuilt}");

    let after_rebuild = history_events(&workspace);
    assert_eq!(after_rebuild.len(), recorded);
    let observed = event_of_kind(&after_rebuild, "commit.observed");
    assert_eq!(observed["message"].as_str(), Some("Fix seat map rounding"));
    assert!(observed["occurredAt"]
        .as_str()
        .unwrap()
        .starts_with("2026-03-04"));
    // The deleted bundle's event is history too, and no artifact can produce
    // it again.
    assert!(
        after_rebuild
            .iter()
            .any(|event| event["eventId"].as_str() == Some("khist_ghostevent0001")),
        "{after_rebuild:#?}"
    );
}

#[test]
fn artifacts_without_commit_details_still_name_their_commits() {
    let root = unique_temp_dir();
    let workspace = workspace_with_recorded_and_observed_commits(&root);

    // An artifact written by an older Knit: no commitDetails anywhere.
    let mut artifact = bundle_artifact(&workspace, "venue-capacity");
    for node in artifact["nodes"].as_array_mut().unwrap() {
        let Some(changes) = node
            .get_mut("repoChanges")
            .and_then(|changes| changes.as_array_mut())
        else {
            continue;
        };
        for change in changes {
            change.as_object_mut().unwrap().remove("commitDetails");
        }
    }
    write_bundle_artifact(&workspace, "venue-capacity", &artifact);
    fs::remove_file(history_path(&workspace)).unwrap();

    // The commit itself is still in the checkout, so history can still name
    // and time it.
    knit(&workspace, ["history", "refresh"]);
    let events = history_events(&workspace);
    let observed = event_of_kind(&events, "commit.observed");
    assert_eq!(
        observed["message"].as_str(),
        Some("Fix seat map rounding"),
        "{observed}"
    );
    assert!(
        observed["occurredAt"]
            .as_str()
            .unwrap()
            .starts_with("2026-03-04"),
        "{observed}"
    );
}
