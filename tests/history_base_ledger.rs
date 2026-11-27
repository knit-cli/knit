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

#[test]
fn base_ledger_records_direct_commits_and_keeps_open_work_separate() {
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
    knit(&workspace, ["history", "refresh"]);
    let base = events(&workspace, "base");
    assert!(base
        .iter()
        .any(|e| e["kind"] == "base.commit" && e["commit"] == base_sha));
    assert!(!base.iter().any(|e| e["commit"] == ongoing_sha));
    let default_history = knit(&workspace, ["log", "--all", "--json"]);
    let explicit_base = knit(&workspace, ["log", "--all", "--scope", "base", "--json"]);
    assert_eq!(default_history, explicit_base);
    assert!(knit(&checkout, ["log", "--json"]).contains(&ongoing_sha));
    let ongoing = events(&workspace, "ongoing");
    assert!(ongoing.iter().any(|e| e["commit"] == ongoing_sha));
    assert!(!ongoing.iter().any(|e| e["kind"] == "base.commit"));
    let both = events(&workspace, "base-and-ongoing");
    assert!(both.iter().any(|e| e["commit"] == base_sha));
    assert!(both.iter().any(|e| e["commit"] == ongoing_sha));
    let first = fs::read(workspace.join(".knit/history/demo.history.jsonl")).unwrap();
    knit(&workspace, ["history", "refresh"]);
    assert_eq!(
        first,
        fs::read(workspace.join(".knit/history/demo.history.jsonl")).unwrap()
    );
    // Archiving an ongoing bundle must neither turn it into base history nor
    // leave it in the open overlay. Its authoring history remains inspectable.
    knit(
        &workspace,
        ["bundle", "archive", "ongoing-change", "--keep-worktrees"],
    );
    assert!(events(&workspace, "ongoing").is_empty());
    assert!(!events(&workspace, "base")
        .iter()
        .any(|e| e["commit"] == ongoing_sha));
    assert!(events(&workspace, "activity")
        .iter()
        .any(|e| e["commit"] == ongoing_sha));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_base_does_not_fall_back_to_unpushed_local_commits() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    let remote_sha = git(&backend, ["rev-parse", "origin/main"])
        .trim()
        .to_string();
    append_line(&backend.join("app.txt"), "not pushed");
    git(&backend, ["commit", "-am", "Unpushed base change"]);
    let local_sha = git(&backend, ["rev-parse", "HEAD"]).trim().to_string();
    knit(&workspace, ["history", "refresh"]);
    let base = events(&workspace, "base");
    assert!(base.iter().any(|e| e["commit"] == remote_sha));
    assert!(!base.iter().any(|e| e["commit"] == local_sha));
    // Refresh also works without a single bundle in the project.
    git(&backend, ["push", "origin", "main"]);
    knit(&workspace, ["history", "refresh"]);
    assert!(events(&workspace, "base")
        .iter()
        .any(|e| e["commit"] == local_sha));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn configured_base_tracks_external_merges_but_not_an_intermediate_branch() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    let backend = root.join("backend");
    git(&backend, ["branch", "stable"]);
    knit(&workspace, ["project", "set-base", "backend", "stable"]);
    knit(
        &workspace,
        ["bundle", "external merge", "--repo", "backend"],
    );
    let checkout = workspace.join(".knit/worktrees/external-merge/backend");
    append_line(&checkout.join("app.txt"), "external work");
    knit(&checkout, ["commit", "--all", "-m", "External work"]);
    git(&backend, ["checkout", "-b", "staging"]);
    git(
        &backend,
        [
            "merge",
            "--no-ff",
            "knit/external-merge",
            "-m",
            "Stage work",
        ],
    );
    let staging_sha = git(&backend, ["rev-parse", "HEAD"]).trim().to_string();
    knit(&workspace, ["history", "refresh"]);
    assert!(!events(&workspace, "base")
        .iter()
        .any(|e| e["commit"] == staging_sha));
    git(&backend, ["checkout", "stable"]);
    git(
        &backend,
        [
            "merge",
            "--no-ff",
            "knit/external-merge",
            "-m",
            "Integrate work into stable",
        ],
    );
    let merged_sha = git(&backend, ["rev-parse", "HEAD"]).trim().to_string();
    knit(&workspace, ["history", "refresh"]);
    let base = events(&workspace, "base");
    assert!(base
        .iter()
        .any(|e| e["commit"] == merged_sha && e["branch"] == "stable"));
    assert!(!base.iter().any(|e| e["commit"] == staging_sha));
    let display = knit(&workspace, ["log", "--all", "--oneline"]);
    assert!(display.contains("[base]"));
    assert!(!display.contains("[bundle activity]"));
    let combined = knit(
        &workspace,
        ["log", "--all", "--scope", "base-and-ongoing", "--oneline"],
    );
    assert!(combined.contains("[bundle activity]"));
    fs::remove_dir_all(root).unwrap();
}

/// The line of `output` that mentions `needle`, for label assertions.
fn line_with<'a>(output: &'a str, needle: &str) -> &'a str {
    output
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line mentions {needle}:\n{output}"))
}

#[test]
fn log_labels_commit_groups_apart_from_bundle_lifecycle_in_every_view() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    knit(
        &workspace,
        [
            "bundle",
            "label check",
            "--repo",
            "backend",
            "--repo",
            "frontend",
        ],
    );
    let bundle_dir = workspace.join(".knit/worktrees/label-check");
    append_line(&bundle_dir.join("backend/app.txt"), "label work");
    append_line(&bundle_dir.join("frontend/app.txt"), "label work");
    knit(&bundle_dir, ["commit", "--all", "-m", "Label work"]);
    knit(&workspace, ["history", "refresh"]);

    // Inside the bundle a commit group carries no label in either form; only
    // the bundle's lifecycle does.
    for form in [vec!["log"], vec!["log", "--oneline"]] {
        let output = knit(&bundle_dir, form.clone());
        let commit = line_with(&output, "Label work");
        assert!(commit.starts_with("kg_"), "{form:?}: {commit}");
        assert!(
            line_with(&output, "Repo added to bundle").starts_with("[bundle activity] "),
            "{form:?}:\n{output}"
        );
    }

    // Project-wide, bundle work reads `[bundle]`, never as lifecycle, in every
    // scope that shows it, in both forms and in commit and event grouping.
    for scope in ["ongoing", "base-and-ongoing", "activity"] {
        for group in ["commit", "event"] {
            for oneline in [false, true] {
                let mut args = vec!["log", "--all", "--scope", scope, "--group", group];
                if oneline {
                    args.push("--oneline");
                }
                let output = knit(&workspace, args.clone());
                assert!(
                    line_with(&output, "Label work").starts_with("[bundle] "),
                    "{args:?}:\n{output}"
                );
                assert!(
                    line_with(&output, "Repo added to bundle").starts_with("[bundle activity] "),
                    "{args:?}:\n{output}"
                );
                if scope != "ongoing" {
                    assert!(
                        line_with(&output, "Initial backend").starts_with("[base] "),
                        "{args:?}:\n{output}"
                    );
                }
            }
        }
    }

    // A whole-bundle group holds its commits, so it is bundle work too.
    let grouped = knit(
        &workspace,
        [
            "log",
            "--all",
            "--scope",
            "ongoing",
            "--group",
            "bundle",
            "--oneline",
        ],
    );
    assert!(
        line_with(&grouped, "label-check").starts_with("[bundle] "),
        "{grouped}"
    );
    let grouped = knit(&bundle_dir, ["log", "--group", "bundle", "--oneline"]);
    assert!(
        !line_with(&grouped, "label-check").starts_with('['),
        "{grouped}"
    );

    // The per-event listing uses the same words.
    let listing = knit(&workspace, ["history"]);
    assert!(
        line_with(&listing, "Label work").contains("[bundle] "),
        "{listing}"
    );
    assert!(
        line_with(&listing, "Repo added to bundle").contains("[bundle activity] "),
        "{listing}"
    );
    fs::remove_dir_all(root).unwrap();
}
