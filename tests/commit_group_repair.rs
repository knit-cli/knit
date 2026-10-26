//! Commit groups across rewrites Knit did not perform: a rewrite that only
//! changed commit headers keeps its named groups, anything else does not, and
//! a bundle collapsed by the older behaviour can be repaired.
mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    worktree: PathBuf,
    slug: String,
}

impl Fixture {
    fn bundle_path(&self) -> PathBuf {
        self.workspace
            .join(".knit/bundles")
            .join(format!("{}.bundle.json", self.slug))
    }

    fn bundle(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.bundle_path()).unwrap()).unwrap()
    }

    /// Group messages paired with the sole commit SHA each one records.
    fn groups(&self) -> Vec<(String, String)> {
        self.bundle()["commitGroups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| {
                (
                    group["message"].as_str().unwrap().to_string(),
                    group["commits"][0]["sha"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }
}

/// A bundle with two named commit groups in one repo.
fn two_group_bundle(title: &str) -> Fixture {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(&workspace, ["bundle", title, "--repo", "backend"]);
    let slug = title.replace(' ', "-");
    let worktree = workspace
        .join(".knit/worktrees")
        .join(&slug)
        .join("backend");

    append_line(&worktree.join("app.txt"), "first change");
    knit(&worktree, ["commit", "-a", "-m", "First change"]);
    append_line(&worktree.join("app.txt"), "second change");
    knit(&worktree, ["commit", "-a", "-m", "Second change"]);

    Fixture {
        root,
        workspace,
        worktree,
        slug,
    }
}

/// Replay `base..HEAD` onto the same base so every commit keeps its tree,
/// author and subject but gets a new identity — the shape of a re-signing or
/// re-dating pass done outside Knit.
fn rewrite_headers_in_place(worktree: &Path, base: &str) {
    let mut command = Command::new("git");
    command
        .args(["rebase", "--force-rebase", base])
        .current_dir(worktree)
        .env("GIT_EDITOR", "true")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        // A fixed committer date guarantees new SHAs even when the original
        // commits were made in this same second.
        .env("GIT_COMMITTER_DATE", "2030-01-01T00:00:00Z");
    run(command);
}

fn recorded_base(fixture: &Fixture) -> String {
    fixture.bundle()["repos"][0]["baseSha"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_content_identical_rewrite_keeps_its_commit_groups() {
    let fixture = two_group_bundle("header rewrite work");
    let before = fixture.groups();
    assert_eq!(before.len(), 2);
    let subjects_before = git(&fixture.worktree, ["log", "--format=%s", "-2"]);
    let base = recorded_base(&fixture);

    rewrite_headers_in_place(&fixture.worktree, &base);
    let output = knit(&fixture.worktree, ["sync"]);
    assert!(output.contains("backend"), "{output}");

    // The work is unchanged and both names survived, on the new SHAs.
    assert_eq!(
        git(&fixture.worktree, ["log", "--format=%s", "-2"]),
        subjects_before
    );
    let after = fixture.groups();
    assert_eq!(after.len(), 2, "{:?}", fixture.bundle()["commitGroups"]);
    for (index, (message, sha)) in before.iter().enumerate() {
        assert_eq!(&after[index].0, message);
        assert_ne!(&after[index].1, sha);
    }
    let head = git(&fixture.worktree, ["rev-parse", "HEAD"]);
    assert_eq!(after[1].1, head.trim());

    // `knit log` shows the named groups rather than anonymous observed work.
    let log = knit(&fixture.worktree, ["log"]);
    assert!(log.contains("First change"), "{log}");
    assert!(log.contains("Second change"), "{log}");
    assert!(!log.contains("observed work"), "{log}");

    fs::remove_dir_all(fixture.root).unwrap();
}

#[test]
fn an_amended_commit_still_retires_its_group() {
    let fixture = two_group_bundle("amended work");
    // Changing the content is not a header-only rewrite, so the old group
    // cannot be claimed to describe the new commit.
    append_line(&fixture.worktree.join("app.txt"), "amended content");
    git(&fixture.worktree, ["add", "app.txt"]);
    git(&fixture.worktree, ["commit", "--amend", "--no-edit"]);

    knit(&fixture.worktree, ["sync"]);

    let after = fixture.bundle();
    let groups = after["commitGroups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{after}");
    assert_eq!(groups[0]["message"], "First change", "{after}");
    let rewrite = after["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node.get("rewrite"))
        .next_back()
        .expect("an observed rewrite");
    assert_eq!(rewrite["kind"], "observed", "{after}");
    assert_eq!(rewrite["supersededGroups"][0]["message"], "Second change");

    fs::remove_dir_all(fixture.root).unwrap();
}

/// Rewrite the commits' headers in place, then reproduce what the older
/// reconcile wrote: one `git.observed` node that superseded every named group,
/// with the names kept inside it. Returns the groups before the collapse and
/// the new SHAs of the first and second commit.
fn collapse_groups(fixture: &Fixture) -> (Vec<(String, String)>, String, String) {
    let before = fixture.groups();
    let base = recorded_base(fixture);
    rewrite_headers_in_place(&fixture.worktree, &base);
    let new_head = git(&fixture.worktree, ["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let new_first = git(&fixture.worktree, ["rev-parse", "HEAD~1"])
        .trim()
        .to_string();

    let mut bundle = fixture.bundle();
    let superseded: Vec<Value> = bundle["commitGroups"].as_array().unwrap().clone();
    bundle["commitGroups"] = json!([]);
    bundle["repos"][0]["headSha"] = json!(new_head);
    let commit_details: Value = superseded
        .iter()
        .map(|group| {
            (
                group["commits"][0]["sha"].as_str().unwrap().to_string(),
                json!({
                    "subject": group["message"],
                    "authoredAt": "2026-01-01T00:00:00Z",
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    let node = json!({
        "id": "git_collapsed",
        "type": "git.observed",
        "createdAt": "2026-01-01T00:00:00Z",
        "commits": [],
        "repoChanges": [{
            "repoId": "backend",
            "movement": "diverged",
            "beforeSha": before[1].1,
            "afterSha": new_head,
            "commits": [new_first, new_head],
            "droppedCommits": [before[0].1, before[1].1],
            "commitDetails": commit_details,
        }],
        "rewrite": {"kind": "observed", "supersededGroups": superseded},
    });
    bundle["nodes"].as_array_mut().unwrap().push(node);
    bundle["headNodeId"] = json!("git_collapsed");
    fs::write(
        fixture.bundle_path(),
        serde_json::to_string_pretty(&bundle).unwrap(),
    )
    .unwrap();
    (before, new_first, new_head)
}

#[test]
fn repair_groups_restores_superseded_names_onto_the_current_shas() {
    let fixture = two_group_bundle("collapsed work");
    let (before, new_first, new_head) = collapse_groups(&fixture);

    // A preview explains the mapping and writes nothing.
    let preview = knit(&fixture.worktree, ["sync", "--repair-groups"]);
    assert!(preview.contains("Restore group"), "{preview}");
    assert!(preview.contains("First change"), "{preview}");
    assert!(preview.contains("Second change"), "{preview}");
    assert!(preview.contains("--apply"), "{preview}");
    assert!(fixture.groups().is_empty());

    let applied = knit(&fixture.worktree, ["sync", "--repair-groups", "--apply"]);
    assert!(applied.contains("restored"), "{applied}");
    assert!(
        applied.contains("knit sync push --bundles --force-with-lease"),
        "{applied}"
    );

    let restored = fixture.groups();
    assert_eq!(restored.len(), 2, "{:?}", fixture.bundle()["commitGroups"]);
    assert_eq!(restored[0].0, before[0].0);
    assert_eq!(restored[0].1, new_first);
    assert_eq!(restored[1].0, before[1].0);
    assert_eq!(restored[1].1, new_head);

    // Idempotent: the restored groups are current, so there is nothing left.
    let again = knit(&fixture.worktree, ["sync", "--repair-groups", "--apply"]);
    assert!(again.contains("No superseded commit groups"), "{again}");
    assert_eq!(fixture.groups().len(), 2);

    fs::remove_dir_all(fixture.root).unwrap();
}

#[test]
fn repair_groups_skips_groups_restored_under_another_creation_time() {
    let fixture = two_group_bundle("hand restored work");
    let (_, new_first, new_head) = collapse_groups(&fixture);
    knit(&fixture.worktree, ["sync", "--repair-groups", "--apply"]);

    // A restore done by hand stamps the groups with the time it ran, not the
    // original creation time. Their commits are current all the same.
    let mut bundle = fixture.bundle();
    for group in bundle["commitGroups"].as_array_mut().unwrap() {
        group["createdAt"] = json!("2030-06-01T00:00:00.000Z");
    }
    fs::write(
        fixture.bundle_path(),
        serde_json::to_string_pretty(&bundle).unwrap(),
    )
    .unwrap();

    let again = knit(&fixture.worktree, ["sync", "--repair-groups", "--apply"]);
    assert!(again.contains("No superseded commit groups"), "{again}");
    let groups = fixture.groups();
    assert_eq!(groups.len(), 2, "{:?}", fixture.bundle()["commitGroups"]);
    assert_eq!(groups[0].1, new_first);
    assert_eq!(groups[1].1, new_head);

    fs::remove_dir_all(fixture.root).unwrap();
}
