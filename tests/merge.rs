mod common;

use common::*;
use serde_json::Value;
use std::fs;

#[test]
fn merge_bundle_into_branch_rolls_back_on_conflict_by_default() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    git(&backend, ["branch", "staging"]);

    knit(&workspace, ["bundle", "feature x"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    fs::write(
        workspace.join(".knit/worktrees/feature-x/backend/app.txt"),
        "feature x\n",
    )
    .unwrap();
    knit(
        &workspace,
        [
            "--bundle",
            "feature-x",
            "commit",
            "--all",
            "-m",
            "Feature X",
        ],
    );

    knit(&workspace, ["bundle", "feature y"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    fs::write(
        workspace.join(".knit/worktrees/feature-y/backend/app.txt"),
        "feature y\n",
    )
    .unwrap();
    knit(
        &workspace,
        [
            "--bundle",
            "feature-y",
            "commit",
            "--all",
            "-m",
            "Feature Y",
        ],
    );

    let first_merge = knit(&workspace, ["merge", "feature-x", "--into", "staging"]);
    assert!(first_merge.contains("Merged"));
    let staging = workspace.join(".knit/merge-worktrees/staging/backend");
    assert_eq!(
        fs::read_to_string(staging.join("app.txt")).unwrap(),
        "feature x\n"
    );

    let failed = knit_fails(&workspace, ["merge", "feature-y", "--into", "staging"]);
    assert!(failed.contains("Merge aborted and this run was rolled back"));
    assert_eq!(
        fs::read_to_string(staging.join("app.txt")).unwrap(),
        "feature x\n"
    );
    assert!(git(&staging, ["diff", "--name-only", "--diff-filter=U"])
        .trim()
        .is_empty());
    assert!(git(&staging, ["status", "--porcelain"]).trim().is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn merge_manual_conflict_can_continue_and_merge_can_target_bundle() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    git(&backend, ["branch", "staging"]);

    knit(&workspace, ["bundle", "feature x"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    fs::write(
        workspace.join(".knit/worktrees/feature-x/backend/app.txt"),
        "feature x\n",
    )
    .unwrap();
    knit(
        &workspace,
        [
            "--bundle",
            "feature-x",
            "commit",
            "--all",
            "-m",
            "Feature X",
        ],
    );

    knit(&workspace, ["bundle", "feature y"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    fs::write(
        workspace.join(".knit/worktrees/feature-y/backend/app.txt"),
        "feature y\n",
    )
    .unwrap();
    knit(
        &workspace,
        [
            "--bundle",
            "feature-y",
            "commit",
            "--all",
            "-m",
            "Feature Y",
        ],
    );

    knit(&workspace, ["merge", "feature-x", "--into", "staging"]);
    let staging = workspace.join(".knit/merge-worktrees/staging/backend");
    let manual = knit_fails(
        &workspace,
        ["merge", "feature-y", "--into", "staging", "--manual"],
    );
    assert!(manual.contains("manual conflict resolution"));
    assert!(manual.contains("Next:"));
    assert!(!git(&staging, ["diff", "--name-only", "--diff-filter=U"])
        .trim()
        .is_empty());
    let status = knit(&workspace, ["merge", "status"]);
    assert!(status.contains("Merge run"));
    assert!(status.contains("feature-y"));
    assert!(status.contains("knit merge --continue"));
    let quiet_status = knit_with_env(&workspace, ["merge", "status"], &[("KNIT_ADVICE", "0")]);
    assert!(!quiet_status.contains("Next:"));
    let show = knit(&workspace, ["merge", "show"]);
    assert!(show.contains("\"kind\": \"KnitMergeRun\""));

    fs::write(staging.join("app.txt"), "resolved staging\n").unwrap();
    git(&staging, ["add", "app.txt"]);
    let continued = knit(&workspace, ["merge", "--continue"]);
    assert!(continued.contains("resolved"));
    assert!(continued.contains("Merged"));
    assert_eq!(
        fs::read_to_string(staging.join("app.txt")).unwrap(),
        "resolved staging\n"
    );

    knit(&workspace, ["bundle", "x y compat"]);
    knit(
        &workspace,
        [
            "--bundle",
            "x-y-compat",
            "bundle",
            "add",
            backend.to_str().unwrap(),
        ],
    );
    assert!(workspace
        .join(".knit/worktrees/x-y-compat/backend")
        .exists());
    let compat_merge = knit(&workspace, ["merge", "feature-x", "--into", "x-y-compat"]);
    assert!(compat_merge.contains("Merged"));
    assert_eq!(
        fs::read_to_string(workspace.join(".knit/worktrees/x-y-compat/backend/app.txt")).unwrap(),
        "feature x\n"
    );
    let compat_conflict = knit_fails(
        &workspace,
        ["merge", "feature-y", "--into", "x-y-compat", "--manual"],
    );
    assert!(compat_conflict.contains("manual conflict resolution"));
    let compat_checkout = workspace.join(".knit/worktrees/x-y-compat/backend");
    fs::write(compat_checkout.join("app.txt"), "feature x + feature y\n").unwrap();
    git(&compat_checkout, ["add", "app.txt"]);
    git(&compat_checkout, ["commit", "-m", "Resolve compat"]);
    let compat_continued = knit(&workspace, ["merge", "--continue"]);
    assert!(compat_continued.contains("resolved"));
    assert!(compat_continued.contains("Merged"));
    assert_eq!(
        fs::read_to_string(compat_checkout.join("app.txt")).unwrap(),
        "feature x + feature y\n"
    );

    let compat_bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/x-y-compat.bundle.json")).unwrap(),
    )
    .unwrap();
    assert!(compat_bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["type"] == "git.observed"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn merge_fetch_push_status_and_target_locks_work() {
    let root = unique_temp_dir();
    let (remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    git(&backend, ["branch", "staging"]);
    git(&backend, ["push", "origin", "staging"]);
    git(&collaborator, ["fetch", "origin", "staging"]);
    git(
        &collaborator,
        ["checkout", "-b", "staging", "origin/staging"],
    );
    append_line(&collaborator.join("app.txt"), "remote staging base");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote staging"]);
    git(&collaborator, ["push", "origin", "staging"]);

    knit(&workspace, ["bundle", "merge push"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    fs::write(
        workspace.join(".knit/worktrees/merge-push/backend/feature.txt"),
        "feature merge push\n",
    )
    .unwrap();
    knit(
        &workspace,
        ["commit", "--all", "-m", "Feature merge push"],
    );

    fs::create_dir_all(workspace.join(".knit/locks")).unwrap();
    fs::write(workspace.join(".knit/locks/merge-staging-backend.lock"), "").unwrap();
    let locked = knit_fails(
        &workspace,
        ["merge", "merge-push", "--into", "staging", "--fetch"],
    );
    assert!(locked.contains("Another Knit process"));
    fs::remove_file(workspace.join(".knit/locks/merge-staging-backend.lock")).unwrap();

    let merged = knit(
        &workspace,
        [
            "merge",
            "merge-push",
            "--into",
            "staging",
            "--fetch",
            "--push",
        ],
    );
    assert!(merged.contains("pushed"));
    let staging = workspace.join(".knit/merge-worktrees/staging/backend");
    let staging_text = fs::read_to_string(staging.join("app.txt")).unwrap();
    assert!(staging_text.contains("remote staging base"));
    assert_eq!(
        fs::read_to_string(staging.join("feature.txt")).unwrap(),
        "feature merge push\n"
    );
    assert_eq!(
        git(&remote, ["rev-parse", "refs/heads/staging"]),
        git(&staging, ["rev-parse", "HEAD"])
    );
    let status = knit(&workspace, ["merge", "status"]);
    assert!(status.contains("pushed"));
    assert!(status.contains("backend"));

    fs::remove_dir_all(root).unwrap();
}

