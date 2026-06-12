mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn setup_project_workspace(root: &Path) -> (PathBuf, PathBuf) {
    let repo = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&repo, "backend");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", repo.to_str().unwrap()],
    );
    knit(&workspace, ["bundle", "venue capacity"]);
    (workspace, repo)
}

#[test]
fn check_record_pins_heads_and_status_tracks_freshness() {
    let root = unique_temp_dir();
    let (workspace, _repo) = setup_project_workspace(&root);

    let recorded = knit(
        &workspace,
        [
            "check",
            "record",
            "ci",
            "--pass",
            "--detail",
            "ran elsewhere",
        ],
    );
    assert!(recorded.contains("ci"), "unexpected output: {recorded}");
    assert!(recorded.contains("green"), "unexpected output: {recorded}");

    let status = knit(&workspace, ["check", "status"]);
    assert!(status.contains("ci"), "unexpected status: {status}");
    assert!(status.contains("green"), "unexpected status: {status}");
    assert!(status.contains("fresh"), "unexpected status: {status}");

    // The verdict is pinned: a new commit makes it stale.
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "moves the bundle",
    );
    knit(&workspace, ["commit", "--all", "-m", "Move the bundle"]);
    let status = knit(&workspace, ["check", "status"]);
    assert!(status.contains("stale"), "expected stale: {status}");

    // Re-recording refreshes it.
    knit(&workspace, ["check", "record", "ci", "--pass"]);
    let status = knit(&workspace, ["check", "status"]);
    assert!(status.contains("fresh"), "expected fresh: {status}");

    // The ledger records every verdict and still validates.
    let log = knit(&workspace, ["log"]);
    assert!(log.contains("check ci"), "expected check in log: {log}");
    let valid = knit(&workspace, ["bundle", "validate"]);
    assert!(valid.contains("Bundle valid"), "unexpected: {valid}");

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/venue-capacity.bundle.json")).unwrap(),
    )
    .unwrap();
    let checks: Vec<&Value> = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "check.recorded")
        .collect();
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0]["title"], "ci");
    assert!(checks[0]["message"].as_str().unwrap().starts_with("pass"));
    assert_eq!(checks[0]["commits"][0]["repoId"], "backend");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn check_run_records_pass_and_fail_from_project_command() {
    let root = unique_temp_dir();
    let (workspace, _repo) = setup_project_workspace(&root);

    knit(
        &workspace,
        [
            "project",
            "command",
            "set",
            "ci",
            "--repo",
            "backend",
            "--",
            "git",
            "--version",
        ],
    );
    let pass = knit(&workspace, ["check", "run", "ci"]);
    assert!(pass.contains("green"), "unexpected output: {pass}");

    knit(
        &workspace,
        [
            "project",
            "command",
            "set",
            "ci",
            "--repo",
            "backend",
            "--",
            "git",
            "definitely-not-a-real-subcommand",
        ],
    );
    // A failing command is still a recorded verdict, then an error exit.
    let fail = knit_fails(&workspace, ["check", "run", "ci"]);
    assert!(fail.contains("red"), "expected recorded red: {fail}");
    assert!(
        fail.contains("check `ci` failed"),
        "expected failure error: {fail}"
    );

    let status = knit(&workspace, ["check", "status"]);
    assert!(
        status.contains("red"),
        "latest verdict should win: {status}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn record_requires_exactly_one_verdict_flag() {
    let root = unique_temp_dir();
    let (workspace, _repo) = setup_project_workspace(&root);

    let neither = knit_fails(&workspace, ["check", "record", "ci"]);
    assert!(
        neither.contains("--pass or --fail"),
        "unexpected: {neither}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn land_apply_gates_on_required_checks() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend land",
    );
    knit(&workspace, ["commit", "--all", "-m", "Landing change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);

    // Require a check by editing the per-bundle plan, the documented override
    // point for one bundle.
    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    let mut plan: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    plan["requireChecks"] = serde_json::json!(["ci"]);
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    // Missing check blocks apply.
    let missing = knit_fails_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        missing.contains("ci (missing)"),
        "expected missing gate: {missing}"
    );

    // A red check blocks apply.
    knit(&workspace, ["check", "record", "ci", "--fail"]);
    let red = knit_fails_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(red.contains("ci (red)"), "expected red gate: {red}");

    // land check reports the blocked required check.
    let check = knit_with_fake_gh(&workspace, ["land", "check"], &fake_bin, &fake_gh_dir);
    assert!(
        check.contains("Required check:") && check.contains("red"),
        "expected required check row: {check}"
    );
    assert!(
        check.contains("required check(s) not green"),
        "expected blocked summary: {check}"
    );

    // A green-but-stale check blocks apply.
    knit(&workspace, ["check", "record", "ci", "--pass"]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "one more change",
    );
    knit(
        &workspace,
        ["commit", "--all", "-m", "Move past the verdict"],
    );
    let stale = knit_fails_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(stale.contains("ci (stale)"), "expected stale gate: {stale}");

    // --skip-checks is the explicit escape hatch... but use the real path:
    // a fresh green verdict lands.
    knit(&workspace, ["check", "record", "ci", "--pass"]);
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        apply.contains("succeeded"),
        "expected landing to succeed with a fresh green check: {apply}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn skip_checks_bypasses_the_gate() {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend land",
    );
    knit(&workspace, ["commit", "--all", "-m", "Landing change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    let mut plan: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    plan["requireChecks"] = serde_json::json!(["ci"]);
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote", "--skip-checks"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        apply.contains("Skipping required checks"),
        "expected skip notice: {apply}"
    );
    assert!(apply.contains("succeeded"), "expected success: {apply}");

    fs::remove_dir_all(root).unwrap();
}
