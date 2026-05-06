use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn three_repo_feature_flow_creates_reviewable_bundle_nodes() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let scraper = root.join("scraper");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    init_repo(&scraper, "scraper");

    knit(&workspace, ["init", "venue capacity"]);
    knit(
        &workspace,
        [
            "track",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
            scraper.to_str().unwrap(),
        ],
    );

    assert!(workspace
        .join(".knit/worktrees/venue-capacity/backend")
        .exists());
    assert!(workspace
        .join(".knit/worktrees/venue-capacity/frontend")
        .exists());
    assert!(workspace
        .join(".knit/worktrees/venue-capacity/scraper")
        .exists());

    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "capacity backend api",
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"),
        "capacity frontend ui",
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/scraper/app.txt"),
        "capacity scraper feed",
    );
    fs::write(
        workspace.join(".knit/worktrees/venue-capacity/frontend/untracked.txt"),
        "not in git yet\n",
    )
    .unwrap();

    let frontend_git_status = knit(
        &workspace,
        ["git", "status", "--short", frontend.to_str().unwrap()],
    );
    assert!(frontend_git_status.contains("M app.txt"));
    assert!(frontend_git_status.contains("?? untracked.txt"));

    let all_git_status = knit(&workspace, ["git", "status", "--short"]);
    assert!(all_git_status.contains("== backend"));
    assert!(all_git_status.contains("== frontend"));
    assert!(all_git_status.contains("== scraper"));

    let diff_stat = knit(&workspace, ["diff", "--stat"]);
    assert!(diff_stat.contains("== backend"));
    assert!(diff_stat.contains("== frontend"));
    assert!(diff_stat.contains("== scraper"));
    assert!(diff_stat.contains("app.txt"));

    let frontend_diff = knit(&workspace, ["diff", "frontend"]);
    assert!(frontend_diff.contains("capacity frontend ui"));
    assert!(!frontend_diff.contains("untracked.txt"));
    assert!(!frontend_diff.contains("capacity backend api"));

    let stage_output = knit(&workspace, ["add"]);
    assert!(stage_output.contains("backend: staged"));
    assert!(stage_output.contains("frontend: staged"));
    assert!(stage_output.contains("scraper: staged"));

    knit(
        &workspace,
        ["commit", "-m", "Add venue capacity integration"],
    );
    let log_output = knit(&workspace, ["log"]);
    assert!(log_output.contains("Add venue capacity integration"));
    assert!(log_output.contains("backend"));
    assert!(log_output.contains("frontend"));
    assert!(log_output.contains("scraper"));
    let show_head = knit(&workspace, ["show", "HEAD"]);
    assert!(show_head.contains("commit.group"));
    assert!(show_head.contains("Add venue capacity integration"));
    assert!(show_head.contains("backend"));
    assert!(show_head.contains("app.txt"));

    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"),
        "manual frontend polish",
    );
    git(
        &workspace.join(".knit/worktrees/venue-capacity/frontend"),
        ["add", "app.txt"],
    );
    git(
        &workspace.join(".knit/worktrees/venue-capacity/frontend"),
        ["commit", "-m", "Manual frontend polish"],
    );
    let raw_frontend_sha = git(
        &workspace.join(".knit/worktrees/venue-capacity/frontend"),
        ["rev-parse", "HEAD"],
    );

    let status_output = knit(&workspace, ["status"]);
    assert!(status_output.contains("frontend"));
    assert!(status_output.contains("unrecorded commits: 1"));

    let sync_output = knit(&workspace, ["sync"]);
    assert!(sync_output.contains("frontend: observed 1 unrecorded commit(s)"));
    let observed_log = knit(&workspace, ["log"]);
    assert!(observed_log.contains("observed git changes"));
    assert!(observed_log.contains("frontend"));
    assert!(observed_log.contains(&raw_frontend_sha[..7]));
    let observed_show = knit(&workspace, ["show", &raw_frontend_sha[..7]]);
    assert!(observed_show.contains("git.observed"));
    assert!(observed_show.contains("Manual frontend polish"));
    assert!(observed_show.contains(&raw_frontend_sha[..7]));
    let previous_show = knit(&workspace, ["show", "HEAD~1"]);
    assert!(previous_show.contains("commit.group"));
    assert!(previous_show.contains("Add venue capacity integration"));
    let limited_log = knit(&workspace, ["log", "-n", "1"]);
    assert!(limited_log.contains("observed git changes"));
    assert!(!limited_log.contains("Add venue capacity integration"));
    let shorthand_log = knit(&workspace, ["log", "-1"]);
    assert!(shorthand_log.contains("observed git changes"));
    assert!(!shorthand_log.contains("Add venue capacity integration"));

    knit(&workspace, ["remove", "scraper"]);

    let bundle = read_bundle(&workspace);
    assert_eq!(bundle["kind"], "ChangeGroup");
    assert_eq!(bundle["repos"].as_array().unwrap().len(), 2);
    for repo in bundle["repos"].as_array().unwrap() {
        assert!(repo["baseSha"].as_str().is_some());
        assert!(repo["headSha"].as_str().is_some());
    }
    assert!(workspace
        .join(".knit/worktrees/venue-capacity/scraper")
        .exists());

    let node_types = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        node_types,
        vec![
            "feature.created",
            "repo.added",
            "worktree.materialized",
            "commit.group",
            "git.observed",
            "repo.removed",
        ]
    );
    let observed = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["type"] == "git.observed")
        .unwrap();
    assert_eq!(
        observed["repoChanges"][0]["repoId"].as_str(),
        Some("frontend")
    );
    assert_eq!(
        observed["repoChanges"][0]["movement"].as_str(),
        Some("advanced")
    );
    assert_eq!(
        observed["repoChanges"][0]["commits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    git(
        &workspace.join(".knit/worktrees/venue-capacity/frontend"),
        ["reset", "--hard", "HEAD~1"],
    );

    let rewind_status = knit(&workspace, ["status"]);
    assert!(rewind_status.contains("frontend"));
    assert!(rewind_status.contains("rewound commits: 1"));

    let rewind_sync = knit(&workspace, ["sync"]);
    assert!(rewind_sync.contains("frontend: observed rewind removing 1 commit(s)"));
    let rewind_log = knit(&workspace, ["log"]);
    assert!(rewind_log.contains("rewound"));
    assert!(rewind_log.contains(&raw_frontend_sha[..7]));
    let rewind_show = knit(&workspace, ["show", "HEAD"]);
    assert!(rewind_show.contains("git.observed"));
    assert!(rewind_show.contains("rewound"));
    assert!(rewind_show.contains(&raw_frontend_sha[..7]));

    let bundle = read_bundle(&workspace);
    let observed_nodes = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "git.observed")
        .collect::<Vec<_>>();
    assert_eq!(observed_nodes.len(), 2);
    let rewind_observed = observed_nodes[1];
    assert_eq!(
        rewind_observed["repoChanges"][0]["movement"].as_str(),
        Some("rewound")
    );
    assert_eq!(
        rewind_observed["repoChanges"][0]["droppedCommits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let rewind_observed_id = rewind_observed["id"].as_str().unwrap().to_string();

    assert_eq!(
        bundle["headNodeId"].as_str(),
        bundle["nodes"].as_array().unwrap().last().unwrap()["id"].as_str()
    );

    let raw_frontend_target = &raw_frontend_sha[..7];
    let rewind_revert_plan = knit(&workspace, ["revert", raw_frontend_target]);
    assert!(rewind_revert_plan.contains("cherryPick"));
    let rewind_revert_apply = knit(&workspace, ["revert", raw_frontend_target, "--apply"]);
    assert!(rewind_revert_apply.contains("Recorded revert group"));
    assert!(
        fs::read_to_string(workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"))
            .unwrap()
            .contains("manual frontend polish")
    );
    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("revert.group"));
    assert_eq!(
        latest["targetNodeId"].as_str(),
        Some(rewind_observed_id.as_str())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn revert_plans_and_applies_commit_groups_and_observed_git() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["init", "venue capacity"]);
    knit(
        &workspace,
        [
            "track",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );

    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "feature backend",
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"),
        "feature frontend",
    );
    knit(&workspace, ["commit", "--stage", "-m", "Feature change"]);
    let bundle = read_bundle(&workspace);
    let feature_backend_sha = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["type"] == "commit.group")
        .unwrap()["commits"][0]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    let feature_backend_target = &feature_backend_sha[..7];

    let unplanned_apply = knit_fails(&workspace, ["revert", feature_backend_target, "--apply"]);
    assert!(unplanned_apply.contains("No revert plan found"));

    let plan = knit(&workspace, ["revert", feature_backend_target]);
    assert!(plan.contains("Revert plan"));
    assert!(plan.contains("backend"));
    assert!(plan.contains("frontend"));
    assert!(plan.contains(&format!("knit revert {feature_backend_target} --apply")));

    let apply = knit(&workspace, ["revert", feature_backend_target, "--apply"]);
    assert!(apply.contains("Recorded revert group"));
    let log = knit(&workspace, ["log", "-1"]);
    assert!(log.contains("revert"));
    let show_revert = knit(&workspace, ["show", "HEAD"]);
    assert!(show_revert.contains("revert.group"));
    assert!(show_revert.contains("Revert Feature change"));
    assert!(
        !fs::read_to_string(workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"))
            .unwrap()
            .contains("feature backend")
    );
    assert!(
        !fs::read_to_string(workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"))
            .unwrap()
            .contains("feature frontend")
    );

    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"),
        "raw frontend",
    );
    git(
        &workspace.join(".knit/worktrees/venue-capacity/frontend"),
        ["add", "app.txt"],
    );
    git(
        &workspace.join(".knit/worktrees/venue-capacity/frontend"),
        ["commit", "-m", "Raw frontend"],
    );
    let raw_frontend_sha = git(
        &workspace.join(".knit/worktrees/venue-capacity/frontend"),
        ["rev-parse", "HEAD"],
    );
    let raw_frontend_target = &raw_frontend_sha[..7];
    knit(&workspace, ["sync"]);

    let observed_plan = knit(&workspace, ["revert", raw_frontend_target]);
    assert!(observed_plan.contains("observed git changes"));
    assert!(observed_plan.contains("frontend"));
    let observed_apply = knit(&workspace, ["revert", raw_frontend_target, "--apply"]);
    assert!(observed_apply.contains("Recorded revert group"));
    assert!(
        !fs::read_to_string(workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"))
            .unwrap()
            .contains("raw frontend")
    );

    let bundle = read_bundle(&workspace);
    let node_types = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        node_types
            .iter()
            .filter(|node_type| **node_type == "revert.group")
            .count(),
        2
    );

    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("revert.group"));
    assert!(latest["targetNodeId"].as_str().is_some());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_updates_original_base_checkout_and_bundle_base_sha() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "venue capacity"]);
    knit(&workspace, ["track", backend.to_str().unwrap()]);
    let feature_head_before = git(
        &workspace.join(".knit/worktrees/venue-capacity/backend"),
        ["rev-parse", "HEAD"],
    );

    append_line(&collaborator.join("app.txt"), "remote base update");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote base update"]);
    git(&collaborator, ["push", "origin", "main"]);
    let remote_sha = git(&collaborator, ["rev-parse", "HEAD"]);

    let pull = knit(&workspace, ["pull", "backend"]);
    assert!(pull.contains("backend"));
    assert!(pull.contains(&remote_sha[..7]));
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), remote_sha);

    let bundle = read_bundle(&workspace);
    assert_eq!(
        bundle["repos"][0]["baseSha"].as_str(),
        Some(remote_sha.trim())
    );
    assert_eq!(
        git(
            &workspace.join(".knit/worktrees/venue-capacity/backend"),
            ["rev-parse", "HEAD"],
        ),
        feature_head_before
    );

    append_line(&collaborator.join("app.txt"), "second remote base update");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Second remote base update"]);
    git(&collaborator, ["push", "origin", "main"]);
    append_line(&backend.join("app.txt"), "local dirty base checkout");

    let dirty_pull = knit_fails(&workspace, ["pull", "backend"]);
    assert!(dirty_pull.contains("Refusing to pull with uncommitted changes"));
    assert!(dirty_pull.contains("backend"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_feature_checkout_records_observed_git_movement() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "venue capacity"]);
    knit(&workspace, ["track", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    git(&feature, ["push", "-u", "origin", "knit/venue-capacity"]);

    git(
        &collaborator,
        ["fetch", "origin", "knit/venue-capacity:knit/venue-capacity"],
    );
    git(&collaborator, ["checkout", "knit/venue-capacity"]);
    append_line(&collaborator.join("app.txt"), "remote feature update");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote feature update"]);
    git(&collaborator, ["push", "origin", "knit/venue-capacity"]);
    let remote_feature_sha = git(&collaborator, ["rev-parse", "HEAD"]);

    let pull = knit(&workspace, ["pull", "--feature", "backend"]);
    assert!(pull.contains("backend"));
    assert!(pull.contains(&remote_feature_sha[..7]));
    assert!(pull.contains("observed 1 unrecorded commit(s)"));
    assert_eq!(git(&feature, ["rev-parse", "HEAD"]), remote_feature_sha);

    let bundle = read_bundle(&workspace);
    assert_eq!(
        bundle["repos"][0]["headSha"].as_str(),
        Some(remote_feature_sha.trim())
    );
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("git.observed"));
    assert_eq!(latest["repoChanges"][0]["repoId"].as_str(), Some("backend"));
    assert_eq!(
        latest["repoChanges"][0]["movement"].as_str(),
        Some("advanced")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn in_place_repos_operate_in_original_checkout_and_guard_branch() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["init", "venue capacity"]);
    knit(
        &workspace,
        ["track", "--in-place", backend.to_str().unwrap()],
    );

    assert!(!workspace
        .join(".knit/worktrees/venue-capacity/backend")
        .exists());
    assert_eq!(
        git(&backend, ["branch", "--show-current"]).trim(),
        "knit/venue-capacity"
    );

    let bundle = read_bundle(&workspace);
    let repo = &bundle["repos"][0];
    assert_eq!(repo["checkoutMode"].as_str(), Some("inPlace"));
    assert_eq!(repo["worktreePath"].as_str(), repo["path"].as_str());

    append_line(&backend.join("app.txt"), "in-place feature");
    let status = knit(&workspace, ["status"]);
    assert!(status.contains("in-place"));
    assert!(status.contains("modified"));
    let diff = knit(&workspace, ["diff", "--stat", "backend"]);
    assert!(diff.contains("backend"));
    assert!(diff.contains("app.txt"));

    knit(&workspace, ["commit", "--stage", "-m", "In-place feature"]);
    assert!(git(&backend, ["log", "-1", "--pretty=%B"]).contains("In-place feature"));

    git(&backend, ["checkout", "main"]);
    let wrong_branch_status = knit(&workspace, ["status"]);
    assert!(wrong_branch_status.contains("wrong branch"));
    let stage_failure = knit_fails(&workspace, ["stage"]);
    assert!(stage_failure.contains("expected `knit/venue-capacity`"));

    fs::remove_dir_all(root).unwrap();
}

fn unique_temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "knit-smoke-{}-{nanos}-{counter}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn init_repo(path: &Path, label: &str) {
    fs::create_dir_all(path).unwrap();
    git(path, ["init"]);
    git(path, ["checkout", "-b", "main"]);
    git(path, ["config", "user.email", "knit@example.test"]);
    git(path, ["config", "user.name", "Knit Smoke"]);
    fs::write(path.join("app.txt"), format!("{label}\n")).unwrap();
    git(path, ["add", "app.txt"]);
    git(path, ["commit", "-m", &format!("Initial {label}")]);
}

fn init_remote_repo(root: &Path, label: &str) -> (PathBuf, PathBuf, PathBuf) {
    let seed = root.join(format!("{label}-seed"));
    init_repo(&seed, label);

    let remote = root.join(format!("{label}.git"));
    git(
        root,
        [
            "clone",
            "--bare",
            seed.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );

    let local = root.join(label);
    git(
        root,
        ["clone", remote.to_str().unwrap(), local.to_str().unwrap()],
    );
    configure_git_user(&local);

    let collaborator = root.join(format!("{label}-collaborator"));
    git(
        root,
        [
            "clone",
            remote.to_str().unwrap(),
            collaborator.to_str().unwrap(),
        ],
    );
    configure_git_user(&collaborator);

    (remote, local, collaborator)
}

fn configure_git_user(path: &Path) {
    git(path, ["config", "user.email", "knit@example.test"]);
    git(path, ["config", "user.name", "Knit Smoke"]);
}

fn append_line(path: &Path, line: &str) {
    let mut text = fs::read_to_string(path).unwrap();
    text.push_str(line);
    text.push('\n');
    fs::write(path, text).unwrap();
}

fn read_bundle(workspace: &Path) -> Value {
    let path = workspace.join(".knit/bundles/venue-capacity.bundle.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn knit<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command.args(args).current_dir(cwd);
    run(command)
}

fn knit_fails<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command.args(args).current_dir(cwd);
    let output = command.output().unwrap();
    assert!(!output.status.success());
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn git<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    run(command)
}

fn run(mut command: Command) -> String {
    let output = command.output().unwrap();
    if !output.status.success() {
        panic!(
            "command failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).to_string()
}
