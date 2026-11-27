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
    knit(&workspace, ["commit", "--all", "-m", "Feature merge push"]);

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

fn bundle_with_feature_commit(
    root: &std::path::Path,
    backend: &std::path::Path,
) -> std::path::PathBuf {
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
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
    workspace
}

/// A branch target merges in a detached managed checkout. When the local
/// branch exists and no checkout holds it, the merge still moves the branch,
/// so `knit merge --into <local-branch>` without `--push` keeps its meaning.
#[test]
fn a_local_merge_target_moves_when_no_checkout_holds_it() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    init_repo(&backend, "backend");
    git(&backend, ["branch", "staging"]);
    let workspace = bundle_with_feature_commit(&root, &backend);
    let staging_before = git(&backend, ["rev-parse", "refs/heads/staging"]);
    let source_head_before = git(&backend, ["rev-parse", "HEAD"]);

    let merged = knit(&workspace, ["merge", "feature-x", "--into", "staging"]);
    assert!(merged.contains("Merged"), "{merged}");
    assert!(!merged.contains("left alone"), "{merged}");

    let managed = workspace.join(".knit/merge-worktrees/staging/backend");
    assert_eq!(git(&managed, ["branch", "--show-current"]).trim(), "");
    let merge_sha = git(&managed, ["rev-parse", "HEAD"]);
    assert_ne!(merge_sha, staging_before);
    assert_eq!(
        git(&backend, ["rev-parse", "refs/heads/staging"]),
        merge_sha
    );
    assert_eq!(
        git(&backend, ["show", "refs/heads/staging:app.txt"]),
        "feature x\n"
    );

    // The source clone stayed on main where it was.
    assert_eq!(git(&backend, ["branch", "--show-current"]).trim(), "main");
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), source_head_before);

    fs::remove_dir_all(root).unwrap();
}

/// When the user's own clone has the target branch checked out, the merge is
/// made in the detached managed checkout and the clone is left exactly as it
/// was, dirty working tree included, with one line saying so.
#[test]
fn a_local_merge_target_is_left_alone_when_a_checkout_holds_it() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    init_repo(&backend, "backend");
    git(&backend, ["branch", "staging"]);
    let workspace = bundle_with_feature_commit(&root, &backend);

    git(&backend, ["checkout", "staging"]);
    let source_head_before = git(&backend, ["rev-parse", "HEAD"]);
    append_line(&backend.join("app.txt"), "uncommitted local edit");
    let source_app_before = fs::read_to_string(backend.join("app.txt")).unwrap();

    let merged = knit(&workspace, ["merge", "feature-x", "--into", "staging"]);
    assert!(merged.contains("Merged"), "{merged}");
    assert!(
        merged.contains("local staging is checked out in") && merged.contains("left alone"),
        "{merged}"
    );
    assert!(merged.contains("the merge lives only in"), "{merged}");

    let managed = workspace.join(".knit/merge-worktrees/staging/backend");
    assert_eq!(git(&managed, ["branch", "--show-current"]).trim(), "");
    assert_eq!(
        fs::read_to_string(managed.join("app.txt")).unwrap(),
        "feature x\n"
    );

    assert_eq!(
        git(&backend, ["branch", "--show-current"]).trim(),
        "staging"
    );
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), source_head_before);
    assert_eq!(
        git(&backend, ["rev-parse", "refs/heads/staging"]),
        source_head_before
    );
    assert_eq!(
        fs::read_to_string(backend.join("app.txt")).unwrap(),
        source_app_before
    );
    assert!(git(&backend, ["status", "--porcelain"]).contains(" M app.txt"));

    fs::remove_dir_all(root).unwrap();
}

/// The pasteable command inside the `Next:` hint a merge printed for a
/// checked-out target branch.
fn printed_catch_up(output: &str) -> String {
    let line = output
        .lines()
        .find(|line| line.contains("Next: fast-forward the checked-out branch to the merge"))
        .unwrap_or_else(|| panic!("no catch-up hint in:\n{output}"));
    line.split('`')
        .nth(1)
        .unwrap_or_else(|| panic!("no command in {line}"))
        .to_string()
}

/// How Knit prints a path as one shell word: bare when nothing in it is
/// special to a shell, single-quoted otherwise.
fn printed_shell_word(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn recorded_step(workspace: &std::path::Path) -> Value {
    let show: Value = serde_json::from_str(&knit(workspace, ["merge", "show"])).unwrap();
    show["steps"][0].clone()
}

/// A checked-out target is left alone, and Knit prints the exact command that
/// brings it up to the merge. Pasted as printed, with a space in the path, it
/// fast-forwards the user's checkout to the merge commit.
#[cfg(unix)]
#[test]
fn a_checked_out_merge_target_gets_a_pasteable_fast_forward() {
    let root = unique_temp_dir();
    let backend = root.join("my repos").join("backend");
    init_repo(&backend, "backend");
    git(&backend, ["branch", "staging"]);
    let workspace = bundle_with_feature_commit(&root, &backend);
    git(&backend, ["checkout", "staging"]);

    let merged = knit(&workspace, ["merge", "feature-x", "--into", "staging"]);
    let managed = workspace.join(".knit/merge-worktrees/staging/backend");
    let merge_sha = git(&managed, ["rev-parse", "HEAD"]).trim().to_string();
    let command = printed_catch_up(&merged);
    assert_eq!(
        command,
        format!("git -C '{}' merge --ff-only {merge_sha}", backend.display())
    );
    // The hint follows the run's summary line.
    assert!(
        merged.find("Merged:").unwrap() < merged.find("Next:").unwrap(),
        "{merged}"
    );

    let step = recorded_step(&workspace);
    assert_eq!(step["localBranchCheckout"], backend.display().to_string());
    assert_eq!(step["catchUpCommand"], command);

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]).trim(), merge_sha);
    assert_eq!(
        fs::read_to_string(backend.join("app.txt")).unwrap(),
        "feature x\n"
    );

    fs::remove_dir_all(root).unwrap();
}

/// A checked-out target with commits of its own cannot fast-forward, so no
/// fast-forward is offered as the fix; the line names the merge instead.
/// `--fetch` merges onto `origin/staging`, which lacks the local commit.
#[test]
fn a_diverged_checked_out_merge_target_gets_no_fast_forward_hint() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    git(&backend, ["branch", "staging"]);
    git(&backend, ["push", "origin", "staging"]);
    let workspace = bundle_with_feature_commit(&root, &backend);
    git(&backend, ["checkout", "staging"]);
    fs::write(backend.join("notes.txt"), "staging only\n").unwrap();
    git(&backend, ["add", "notes.txt"]);
    git(&backend, ["commit", "-m", "Staging only"]);

    let merged = knit(
        &workspace,
        ["merge", "feature-x", "--into", "staging", "--fetch"],
    );
    assert!(merged.contains("Merged"), "{merged}");
    assert!(
        merged.contains("has commits the merge does not contain")
            && merged.contains("cannot fast-forward"),
        "{merged}"
    );
    assert!(!merged.contains("--ff-only"), "{merged}");
    assert!(!merged.contains("Next:"), "{merged}");

    let step = recorded_step(&workspace);
    assert_eq!(step["localBranchCheckout"], backend.display().to_string());
    assert!(step.get("catchUpCommand").is_none(), "{step}");

    fs::remove_dir_all(root).unwrap();
}

/// `knit merge --continue` finishes a run the same way a clean run does: the
/// checked-out target is left alone and the hint names the resolved merge.
#[test]
fn merge_continue_prints_the_catch_up_for_a_checked_out_target() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    init_repo(&backend, "backend");
    git(&backend, ["branch", "staging"]);
    let workspace = bundle_with_feature_commit(&root, &backend);
    git(&backend, ["checkout", "staging"]);
    // A staging commit the feature conflicts with.
    fs::write(backend.join("app.txt"), "staging\n").unwrap();
    git(&backend, ["commit", "-am", "Staging edit"]);

    knit_fails(
        &workspace,
        ["merge", "feature-x", "--into", "staging", "--manual"],
    );
    let managed = workspace.join(".knit/merge-worktrees/staging/backend");
    fs::write(managed.join("app.txt"), "resolved\n").unwrap();
    git(&managed, ["add", "app.txt"]);
    let continued = knit(&workspace, ["merge", "--continue"]);
    let merge_sha = git(&managed, ["rev-parse", "HEAD"]).trim().to_string();
    assert!(continued.contains("left alone"), "{continued}");
    assert_eq!(
        printed_catch_up(&continued),
        format!(
            "git -C {} merge --ff-only {merge_sha}",
            printed_shell_word(&backend.display().to_string())
        )
    );

    fs::remove_dir_all(root).unwrap();
}
