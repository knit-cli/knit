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
    assert!(display.contains("[bundle activity]"));
    fs::remove_dir_all(root).unwrap();
}
