mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// The same messages in a Git repository and a preserved Knit event ledger.
/// No bundle artifacts exist: inspection must work from recorded history alone.
fn histories() -> (PathBuf, PathBuf) {
    let root = unique_temp_dir();
    let repo = root.join("repo");
    init_repo(&repo, "api");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "demo"]);
    let directory = workspace.join(".knit/history");
    fs::create_dir_all(&directory).unwrap();
    let mut lines = String::new();
    for (index, message) in [
        "Update api endpoint",
        "Update web endpoint",
        "Document api|web selection",
        "Cache [entry] lookup",
        "Improve storage\n\nCache details stay in the commit body.",
    ]
    .into_iter()
    .enumerate()
    {
        git(&repo, ["commit", "--allow-empty", "-m", message]);
        let sha = git(&repo, ["rev-parse", "HEAD"]).trim().to_string();
        let event = json!({
            "schemaVersion": "knit.history.event.v1",
            "eventId": format!("event-{index}"),
            "projectId": "demo",
            "kind": "commit.recorded",
            "bundleId": "recorded-work",
            "bundleTitle": "Recorded work",
            "repoId": "api",
            "nodeId": format!("node-{index}"),
            "nodeType": "commit.group",
            "commit": sha,
            "message": message,
            "occurredAt": format!("2026-01-{:02}T00:00:00Z", index + 1),
            "recordedAt": "2026-02-01T00:00:00Z",
            "recordedBy": "test"
        });
        lines.push_str(&serde_json::to_string(&event).unwrap());
        lines.push('\n');
    }
    fs::write(directory.join("demo.history.jsonl"), lines).unwrap();
    (workspace, repo)
}

fn knit_subjects(workspace: &Path, arguments: &[&str]) -> Vec<String> {
    let mut args = vec!["log", "--all", "--group", "event", "--json"];
    args.extend_from_slice(arguments);
    let output = knit(workspace, args);
    let entries: Vec<Value> = serde_json::from_str(&output).unwrap();
    entries
        .iter()
        .map(|entry| {
            entry["message"]
                .as_str()
                .unwrap()
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[test]
fn grep_matches_git_basic_patterns_and_message_lines() {
    let (workspace, repo) = histories();
    for pattern in ["api|web", r"api\|web", "^Cache", r"Cache \[entry\]"] {
        let expected = git(
            &repo,
            ["log", "HEAD~5..HEAD", "--format=%s", "--grep", pattern],
        )
        .lines()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        assert_eq!(
            knit_subjects(&workspace, &["--grep", pattern]),
            expected,
            "grep pattern {pattern:?} should behave like git log"
        );
    }
}

#[test]
fn fixed_string_search_and_reverse_limit_match_git() {
    let (workspace, repo) = histories();
    let expected = git(
        &repo,
        [
            "log",
            "HEAD~5..HEAD",
            "--format=%s",
            "-F",
            "-i",
            "--grep",
            "cache [ENTRY]",
        ],
    )
    .lines()
    .map(ToString::to_string)
    .collect::<Vec<_>>();
    assert_eq!(
        knit_subjects(&workspace, &["-F", "-i", "--grep", "cache [ENTRY]"]),
        expected
    );
    let expected = git(
        &repo,
        ["log", "HEAD~5..HEAD", "--format=%s", "-n", "2", "--reverse"],
    )
    .lines()
    .map(ToString::to_string)
    .collect::<Vec<_>>();
    assert_eq!(
        knit_subjects(&workspace, &["-n", "2", "--reverse"]),
        expected
    );
}

#[test]
fn failed_query_does_not_change_preserved_history() {
    let (workspace, _) = histories();
    let path = workspace.join(".knit/history/demo.history.jsonl");
    let before = fs::read(&path).unwrap();
    let error = knit_fails(&workspace, ["log", "--all", "--grep", "["]);
    assert!(
        error.contains("grep") || error.contains("pattern"),
        "{error}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(knit_subjects(&workspace, &[]).len(), 5);
}
