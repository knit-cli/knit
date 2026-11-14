//! `knit status` reports each review's last recorded state offline, and its
//! `Next:` line never suggests landing over a review recorded as blocked or
//! one whose state is unknown or old.

mod common;

use chrono::{Duration, SecondsFormat, Utc};
use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const BUNDLE: &str = "venue-capacity";

fn workspace_with(repos: &[&str]) -> PathBuf {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "venue capacity"]);
    for repo in repos {
        let path = root.join(repo);
        init_repo(&path, repo);
        knit(&workspace, ["bundle", "add", path.to_str().unwrap()]);
    }
    write_bundle_publications(&workspace, BUNDLE, "OPEN");
    workspace
}

fn ago(minutes: i64) -> String {
    (Utc::now() - Duration::minutes(minutes)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn met(kind: &str, actor: &str) -> Value {
    json!({"kind": kind, "state": "met", "actor": actor, "summary": format!("{kind} met")})
}

fn conflict() -> Value {
    json!({
        "kind": "conflict",
        "state": "blocked",
        "actor": "you",
        "summary": "conflicts with its base: run `knit land update`"
    })
}

/// Record gates on a repo's review as `knit publish sync` would.
fn record_gates(workspace: &Path, repo: &str, items: Vec<Value>, updated_at: &str) {
    let path = workspace
        .join(".knit/bundles")
        .join(format!("{BUNDLE}.bundle.json"));
    let mut bundle: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let publication = bundle["publications"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["repoId"] == repo)
        .unwrap();
    publication["gates"] = json!({"updatedAt": updated_at, "items": items});
    fs::write(path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();
}

fn clean_gates() -> Vec<Value> {
    vec![met("review", "maintainers"), met("checks", "host")]
}

fn next_line(output: &str) -> &str {
    output
        .lines()
        .find(|line| line.starts_with("Next:"))
        .unwrap_or_else(|| panic!("no Next: line in\n{output}"))
}

fn status_json(workspace: &Path) -> Value {
    serde_json::from_str(&knit(workspace, ["status", "--json"])).expect("status --json")
}

fn review<'a>(document: &'a Value, repo: &str) -> &'a Value {
    &document["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == repo)
        .unwrap()["review"]
}

#[test]
fn a_recorded_conflict_replaces_the_landing_advice() {
    let workspace = workspace_with(&["api", "web", "docs"]);
    record_gates(
        &workspace,
        "api",
        {
            let mut gates = clean_gates();
            gates.insert(0, conflict());
            gates
        },
        &ago(3 * 24 * 60),
    );
    record_gates(&workspace, "docs", clean_gates(), &ago(5));

    let output = knit(&workspace, ["status"]);
    assert!(
        output.contains("Publications: 3/3 review object(s) recorded, not landed"),
        "{output}"
    );
    let api = output
        .lines()
        .find(|l| l.trim_start().starts_with("api ") && l.contains("#1"))
        .unwrap();
    assert!(api.contains("conflict"), "{api}");
    assert!(api.contains("action needed: conflict"), "{api}");
    assert!(api.contains("recorded 3d ago, stale"), "{api}");
    let web = output.lines().find(|l| l.contains("#2")).unwrap();
    assert!(
        web.contains("unknown   no gates recorded; check live"),
        "{web}"
    );
    let docs = output.lines().find(|l| l.contains("#3")).unwrap();
    assert!(
        docs.contains("clean     ready  (recorded 5m ago)"),
        "{docs}"
    );

    let next = next_line(&output);
    assert!(next.contains("do not land yet"), "{next}");
    assert!(next.contains("api #1 conflicts with its base"), "{next}");
    assert!(next.contains("`knit land update --push api`"), "{next}");
    assert!(next.contains("`knit publish status --live`"), "{next}");
    assert!(!next.contains("`knit land apply`"), "{next}");

    let document = status_json(&workspace);
    assert_eq!(document["publications"]["readiness"], "blocked");
    assert_eq!(document["publications"]["freshForSeconds"], 3600);

    let api = review(&document, "api");
    assert_eq!(api["number"], 1);
    assert_eq!(api["state"], "open");
    assert_eq!(api["mergeable"], "conflict");
    assert_eq!(api["blocked"], true);
    assert_eq!(api["stale"], true);
    assert_eq!(api["verdict"], "action needed: conflict");
    assert_eq!(api["openGates"][0]["kind"], "conflict");
    assert!(api["recordedAt"].is_string());
    assert!(api["ageSeconds"].as_i64().unwrap() >= 3 * 24 * 3600);

    let web = review(&document, "web");
    assert_eq!(web["mergeable"], "unknown");
    assert_eq!(web["blocked"], false);
    assert_eq!(web["verdict"], Value::Null);
    assert_eq!(web["recordedAt"], Value::Null);

    let docs = review(&document, "docs");
    assert_eq!(docs["mergeable"], "clean");
    assert_eq!(docs["verdict"], "ready");
    assert_eq!(docs["stale"], false);
    assert_eq!(docs["openGates"], json!([]));
}

#[test]
fn an_unknown_review_state_points_at_the_live_check() {
    let workspace = workspace_with(&["api", "web"]);
    record_gates(&workspace, "web", clean_gates(), &ago(1));

    let output = knit(&workspace, ["status"]);
    let next = next_line(&output);
    assert!(
        next.contains("the recorded review state of api is unknown"),
        "{next}"
    );
    assert!(next.contains("`knit publish status --live`"), "{next}");

    assert_eq!(
        status_json(&workspace)["publications"]["readiness"],
        "unverified"
    );
}

#[test]
fn an_old_clean_reading_points_at_the_live_check() {
    let workspace = workspace_with(&["api"]);
    record_gates(&workspace, "api", clean_gates(), &ago(2 * 60));

    let output = knit(&workspace, ["status"]);
    assert!(output.contains("(recorded 2h ago, stale)"), "{output}");
    assert!(
        next_line(&output).contains("`knit publish status --live`"),
        "{output}"
    );
    let document = status_json(&workspace);
    assert_eq!(document["publications"]["readiness"], "unverified");
    assert_eq!(review(&document, "api")["stale"], true);
}

#[test]
fn recent_clean_readings_keep_the_landing_advice() {
    let workspace = workspace_with(&["api", "web"]);
    record_gates(&workspace, "api", clean_gates(), &ago(2));
    record_gates(&workspace, "web", clean_gates(), &ago(10));

    let output = knit(&workspace, ["status"]);
    assert_eq!(
        next_line(&output),
        "Next: when the user says to land/release, run `knit land` to create or show the plan, then `knit land apply` after inspection; do not merge the host review objects directly."
    );
    assert_eq!(
        status_json(&workspace)["publications"]["readiness"],
        "ready"
    );
}

#[test]
fn status_without_reviews_omits_the_review_fields() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&root.join("api"), "api");
    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        ["bundle", "add", root.join("api").to_str().unwrap()],
    );

    let document = status_json(&workspace);
    assert_eq!(document["publications"]["reviews"], 0);
    assert!(document["publications"].get("readiness").is_none());
    assert!(document["publications"].get("freshForSeconds").is_none());
    assert!(document["repos"][0].get("review").is_none());
}

#[test]
fn offline_publish_status_shows_the_recorded_reading_and_the_same_advice() {
    let workspace = workspace_with(&["api"]);
    record_gates(&workspace, "api", vec![conflict()], &ago(30));

    let output = knit(&workspace, ["publish", "status"]);
    assert!(
        output.contains("recorded: conflict  action needed: conflict  (recorded 30m ago)"),
        "{output}"
    );
    assert!(
        next_line(&output).contains("`knit land update --push api`"),
        "{output}"
    );
}

#[test]
fn a_conflict_synced_from_the_host_reaches_status_and_clears_after_the_next_sync() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);

    // Published without a sync: nothing on record yet.
    let output = knit(&workspace, ["status"]);
    assert!(
        output.contains("unknown   no gates recorded; check live"),
        "{output}"
    );
    let next = next_line(&output);
    assert!(next.contains("of backend, frontend is unknown"), "{next}");
    assert!(next.contains("`knit publish status --live`"), "{next}");

    // The host reports a conflict on frontend; a sync records it.
    fs::write(fake_gh_dir.join("conflict-frontend"), "").unwrap();
    knit_with_fake_gh(&workspace, ["publish", "sync"], &fake_bin, &fake_gh_dir);

    let output = knit(&workspace, ["status"]);
    let frontend = output
        .lines()
        .find(|l| l.trim_start().starts_with("frontend") && l.contains('#'))
        .unwrap_or_else(|| panic!("{output}"));
    assert!(
        frontend.contains("conflict  action needed: conflict"),
        "{frontend}"
    );
    assert!(frontend.contains("recorded just now"), "{frontend}");
    let next = next_line(&output);
    assert!(
        next.contains("frontend #") && next.contains(" conflicts with its base"),
        "{next}"
    );
    assert!(
        next.contains("`knit land update --push frontend`"),
        "{next}"
    );
    assert!(!next.contains("backend"), "{next}");

    let document = status_json(&workspace);
    assert_eq!(document["publications"]["readiness"], "blocked");
    assert_eq!(review(&document, "frontend")["mergeable"], "conflict");
    assert_eq!(review(&document, "frontend")["stale"], false);
    assert_eq!(review(&document, "backend")["mergeable"], "clean");
    assert_eq!(review(&document, "backend")["blocked"], false);

    // The live view agrees and gives the same way out.
    let live = knit_with_fake_gh(
        &workspace,
        ["publish", "status", "--live"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(live.contains("conflicts with its base"), "{live}");
    let next = next_line(&live);
    assert!(next.contains("do not land yet: frontend"), "{next}");
    assert!(
        next.contains("`knit land update --push frontend`"),
        "{next}"
    );

    // The advised command is a real invocation.
    knit_with_fake_gh(
        &workspace,
        ["land", "update", "--push", "frontend"],
        &fake_bin,
        &fake_gh_dir,
    );

    // Once the host reports it clean again, a sync brings back the landing advice.
    fs::remove_file(fake_gh_dir.join("conflict-frontend")).unwrap();
    knit_with_fake_gh(&workspace, ["publish", "sync"], &fake_bin, &fake_gh_dir);
    let output = knit(&workspace, ["status"]);
    assert!(
        next_line(&output).starts_with("Next: when the user says to land/release, run `knit land`"),
        "{output}"
    );
    assert_eq!(
        status_json(&workspace)["publications"]["readiness"],
        "ready"
    );

    let live = knit_with_fake_gh(
        &workspace,
        ["publish", "status", "--live"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        next_line(&live).starts_with("Next: when the user says to land/release"),
        "{live}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_live_blocker_that_is_not_a_conflict_holds_landing_without_land_update() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);

    let live = common::knit_with_fake_gh_env(
        &workspace,
        ["publish", "status", "--live"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_DRAFT", "1")],
    );
    assert!(live.contains("draft: mark it ready for review"), "{live}");
    let next = next_line(&live);
    assert!(
        next.contains("do not land yet: resolve the gates blocked on you"),
        "{next}"
    );
    assert!(!next.contains("land update"), "{next}");

    fs::remove_dir_all(root).unwrap();
}
