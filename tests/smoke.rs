use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn init_can_generate_agents_tutorial() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let output = knit(&workspace, ["init", "venue capacity", "--agents"]);
    assert!(output.contains("AGENTS.md"));

    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("This is a Knit workspace"));
    assert!(agents.contains("knit status"));
    assert!(agents.contains("knit track"));
    assert!(agents.contains("knit commit --stage"));
    assert!(agents.contains("gloss prepare"));

    fs::remove_file(workspace.join("AGENTS.md")).unwrap();
    let existing_workspace_output = knit(&workspace, ["init", "venue capacity", "--agents"]);
    assert!(existing_workspace_output.contains("AGENTS.md"));
    let existing_workspace_agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(existing_workspace_agents.contains("This is a Knit workspace"));

    fs::write(workspace.join("AGENTS.md"), "custom guidance\n").unwrap();
    knit(
        &workspace,
        ["init", "venue capacity", "--force", "--agents"],
    );
    let updated = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(updated.contains("custom guidance"));
    assert!(updated.contains("This is a Knit workspace"));
    assert_eq!(updated.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);

    knit(
        &workspace,
        ["init", "venue capacity", "--force", "--agents"],
    );
    let rerun = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert_eq!(rerun.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);

    fs::remove_dir_all(root).unwrap();
}

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
    let bundle_path = knit(&workspace, ["bundle", "path"]);
    assert!(bundle_path
        .trim_end()
        .ends_with("venue-capacity.bundle.json"));
    let printed_bundle = knit(&workspace, ["bundle", "print"]);
    assert!(printed_bundle.contains("\"kind\": \"ChangeGroup\""));
    assert!(printed_bundle.contains("\"id\": \"venue-capacity\""));
    let valid_bundle = knit(&workspace, ["bundle", "validate"]);
    assert!(valid_bundle.contains("Bundle valid"));
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
fn fetch_updates_remote_refs_without_moving_checkout_or_bundle_base() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "venue capacity"]);
    knit(&workspace, ["track", backend.to_str().unwrap()]);
    let initial_head = git(&backend, ["rev-parse", "HEAD"]);
    let initial_bundle = read_bundle(&workspace);
    let initial_base_sha = initial_bundle["repos"][0]["baseSha"]
        .as_str()
        .unwrap()
        .to_string();

    append_line(&collaborator.join("app.txt"), "remote base fetch");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Remote base fetch"]);
    git(&collaborator, ["push", "origin", "main"]);
    let remote_sha = git(&collaborator, ["rev-parse", "HEAD"]);

    let fetch = knit(&workspace, ["fetch", "backend"]);
    assert!(fetch.contains("backend"));
    assert!(fetch.contains("origin/main"));
    assert!(fetch.contains(&remote_sha[..7]));
    assert_eq!(git(&backend, ["rev-parse", "origin/main"]), remote_sha);
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), initial_head);

    let bundle = read_bundle(&workspace);
    assert_eq!(
        bundle["repos"][0]["baseSha"].as_str(),
        Some(initial_base_sha.as_str())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn push_sends_feature_branch_and_can_set_upstream() {
    let root = unique_temp_dir();
    let (remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "venue capacity"]);
    knit(&workspace, ["track", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");

    append_line(&feature.join("app.txt"), "feature push");
    knit(&workspace, ["commit", "--stage", "-m", "Feature push"]);
    let first_sha = git(&feature, ["rev-parse", "HEAD"]);

    let push = knit(&workspace, ["push", "backend"]);
    assert!(push.contains("backend"));
    assert!(push.contains("origin/knit/venue-capacity"));
    assert!(push.contains(&first_sha[..7]));
    assert_eq!(
        git(&remote, ["rev-parse", "refs/heads/knit/venue-capacity"]),
        first_sha
    );

    append_line(&feature.join("app.txt"), "feature push with upstream");
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Feature push with upstream"],
    );
    let second_sha = git(&feature, ["rev-parse", "HEAD"]);

    let push_upstream = knit(&workspace, ["push", "--set-upstream", "backend"]);
    assert!(push_upstream.contains("backend"));
    assert!(push_upstream.contains(&second_sha[..7]));
    assert_eq!(
        git(&remote, ["rev-parse", "refs/heads/knit/venue-capacity"]),
        second_sha
    );
    assert_eq!(
        git(
            &feature,
            ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .trim(),
        "origin/knit/venue-capacity"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_pushes_creates_records_and_syncs_cross_links() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "venue capacity"]);
    knit(
        &workspace,
        [
            "track",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );

    let backend_feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    let frontend_feature = workspace.join(".knit/worktrees/venue-capacity/frontend");
    append_line(&backend_feature.join("app.txt"), "backend PR change");
    append_line(&frontend_feature.join("app.txt"), "frontend PR change");
    knit(&workspace, ["commit", "--stage", "-m", "PR change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);

    let create = knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--draft"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert!(create.contains("frontend"));
    assert!(create.contains("created"));
    assert!(create.contains("synced"));

    assert_eq!(
        git(
            &backend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        git(&backend_feature, ["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(
            &frontend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        git(&frontend_feature, ["rev-parse", "HEAD"])
    );

    let bundle = read_bundle(&workspace);
    let publications = bundle["publications"].as_array().unwrap();
    assert_eq!(publications.len(), 2);
    assert_eq!(publications[0]["provider"].as_str(), Some("github"));
    assert_eq!(publications[0]["kind"].as_str(), Some("pull_request"));
    assert!(publications
        .iter()
        .any(|publication| publication["url"] == "https://github.com/acme/backend/pull/101"));
    assert!(publications
        .iter()
        .any(|publication| publication["url"] == "https://github.com/acme/frontend/pull/202"));

    let backend_body = fs::read_to_string(fake_gh_dir.join("edit-backend.md")).unwrap();
    assert!(backend_body.contains("This PR is part of Knit bundle `venue-capacity`."));
    assert!(backend_body.contains("`backend`: https://github.com/acme/backend/pull/101 (this PR)"));
    assert!(backend_body.contains("`frontend`: https://github.com/acme/frontend/pull/202"));

    let frontend_body = fs::read_to_string(fake_gh_dir.join("edit-frontend.md")).unwrap();
    assert!(frontend_body.contains("`backend`: https://github.com/acme/backend/pull/101"));
    assert!(
        frontend_body.contains("`frontend`: https://github.com/acme/frontend/pull/202 (this PR)")
    );

    let status = knit(&workspace, ["publish", "github", "status"]);
    assert!(status.contains("#101"));
    assert!(status.contains("#202"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_plan_and_apply_merges_recorded_publications_with_fake_gh() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

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
        "backend land",
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"),
        "frontend land",
    );
    knit(&workspace, ["commit", "--stage", "-m", "Landing change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let missing_plan =
        knit_fails_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(missing_plan.contains("No land plan found"));

    let plan = knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);
    assert!(plan.contains("Land plan"));
    assert!(plan.contains("merge-backend"));
    assert!(plan.contains("merge-frontend"));
    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    assert!(plan_path.exists());

    let apply = knit_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(apply.contains("Feature landed"));
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(
        order.lines().collect::<Vec<_>>(),
        vec!["backend", "frontend"]
    );

    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("feature.landed"));
    assert_eq!(latest["provider"].as_str(), Some("github"));
    assert_eq!(latest["repoIds"].as_array().unwrap().len(), 2);
    assert_eq!(latest["publicationUrls"].as_array().unwrap().len(), 2);
    assert!(workspace.join(".knit/land-runs").exists());
    assert!(knit(&workspace, ["bundle", "validate"]).contains("Bundle valid"));
    assert!(knit(&workspace, ["log", "-1"]).contains("landed"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_resume_skips_succeeded_steps_and_retries_failed_run_steps() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "venue capacity"]);
    knit(&workspace, ["track", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend land resume",
    );
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Landing resume change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);

    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    let mut plan: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    plan["steps"].as_array_mut().unwrap().push(json!({
        "id": "deploy",
        "type": "run",
        "cwd": ".",
        "command": ["sh", "-c", "test \"$DEPLOY_OK\" = \"yes\" && test -f deploy-ok"],
        "env": { "DEPLOY_OK": "yes" },
        "needs": ["merge-backend"]
    }));
    fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    let failed = knit_fails_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(failed.contains("stopped at step deploy"));
    let bundle_after_failure = read_bundle(&workspace);
    assert_ne!(
        bundle_after_failure["nodes"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["type"]
            .as_str(),
        Some("feature.landed")
    );

    fs::write(workspace.join("deploy-ok"), "ready\n").unwrap();
    let resumed = knit_with_fake_gh(&workspace, ["land", "resume"], &fake_bin, &fake_gh_dir);
    assert!(resumed.contains("Feature landed"));
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(order.lines().collect::<Vec<_>>(), vec!["backend"]);
    let status = knit_with_fake_gh(&workspace, ["land", "status"], &fake_bin, &fake_gh_dir);
    assert!(status.contains("succeeded"));
    assert!(status.contains("deploy"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_refuses_draft_publications() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "venue capacity"]);
    knit(&workspace, ["track", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend draft land",
    );
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Draft landing change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);

    let failed = knit_fails_with_fake_gh_env(
        &workspace,
        ["land", "apply"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_DRAFT", "1")],
    );
    assert!(failed.contains("is a draft"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_stops_when_required_checks_fail() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "venue capacity"]);
    knit(&workspace, ["track", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend check failure",
    );
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Check failure landing"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(&workspace, ["land", "plan"], &fake_bin, &fake_gh_dir);

    let failed = knit_fails_with_fake_gh_env(
        &workspace,
        ["land", "apply"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_CHECKS_FAIL", "1")],
    );
    assert!(failed.contains("check `test` failed"));
    assert!(!fake_gh_dir.join("merge-order.txt").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn checkpoint_records_non_git_ledger_note() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "venue capacity"]);
    let output = knit(
        &workspace,
        ["checkpoint", "frontend wired, backend pending"],
    );
    assert!(output.contains("Recorded checkpoint"));

    let log = knit(&workspace, ["log", "-1"]);
    assert!(log.contains("checkpoint"));
    assert!(log.contains("frontend wired, backend pending"));

    let show = knit(&workspace, ["show", "HEAD"]);
    assert!(show.contains("checkpoint"));
    assert!(show.contains("frontend wired, backend pending"));

    let valid = knit(&workspace, ["bundle", "validate"]);
    assert!(valid.contains("Bundle valid"));

    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("checkpoint"));
    assert_eq!(
        bundle["headNodeId"].as_str(),
        Some(latest["id"].as_str().unwrap())
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn close_records_feature_closed_node_without_git_state() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["init", "venue capacity"]);
    knit(&workspace, ["track", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    let head_before_close = git(&feature, ["rev-parse", "HEAD"]);

    let close = knit(&workspace, ["close", "--reason", "merged"]);
    assert!(close.contains("Closed bundle"));

    let log = knit(&workspace, ["log", "-1"]);
    assert!(log.contains("closed"));
    assert!(log.contains("merged"));

    let show = knit(&workspace, ["show", "HEAD"]);
    assert!(show.contains("feature.closed"));
    assert!(show.contains("merged"));

    let valid = knit(&workspace, ["bundle", "validate"]);
    assert!(valid.contains("Bundle valid"));
    assert_eq!(git(&feature, ["rev-parse", "HEAD"]), head_before_close);

    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("feature.closed"));
    assert_eq!(latest["message"].as_str(), Some("merged"));
    assert_eq!(
        bundle["headNodeId"].as_str(),
        Some(latest["id"].as_str().unwrap())
    );

    let second_close = knit_fails(&workspace, ["close"]);
    assert!(second_close.contains("already closed"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn clean_removes_plans_and_generated_worktrees_only() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["init", "venue capacity"]);
    knit(&workspace, ["track", backend.to_str().unwrap()]);
    let worktree = workspace.join(".knit/worktrees/venue-capacity/backend");

    append_line(&worktree.join("app.txt"), "clean test change");
    knit(&workspace, ["commit", "--stage", "-m", "Clean test change"]);
    knit(&workspace, ["revert", "HEAD"]);
    assert!(workspace.join(".knit/revert-plans").exists());

    let no_target = knit_fails(&workspace, ["clean"]);
    assert!(no_target.contains("Choose what to clean"));

    let clean_plans = knit(&workspace, ["clean", "--plans"]);
    assert!(clean_plans.contains("removed"));
    assert!(!workspace.join(".knit/revert-plans").exists());

    let clean_worktrees = knit(&workspace, ["clean", "--worktrees"]);
    assert!(clean_worktrees.contains("backend"));
    assert!(clean_worktrees.contains("removed"));
    assert!(!worktree.exists());
    assert!(backend.exists());
    assert!(
        git(&backend, ["branch", "--list", "knit/venue-capacity"]).contains("knit/venue-capacity")
    );

    let bundle = read_bundle(&workspace);
    assert!(bundle["repos"][0]["worktreePath"].is_null());
    let valid = knit(&workspace, ["bundle", "validate"]);
    assert!(valid.contains("Bundle valid"));
    let status_after_clean = knit(&workspace, ["status"]);
    assert!(status_after_clean.contains("(not materialized)"));
    assert!(status_after_clean.contains("missing worktree"));
    let git_after_clean = knit_fails(&workspace, ["git", "status", "--short", "backend"]);
    assert!(git_after_clean.contains("has no active checkout"));

    knit(&workspace, ["worktree"]);
    assert!(worktree.exists());

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

fn knit_with_fake_gh<I, S>(cwd: &Path, args: I, fake_bin: &Path, fake_gh_dir: &Path) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    knit_with_fake_gh_env(cwd, args, fake_bin, fake_gh_dir, &[])
}

fn knit_with_fake_gh_env<I, S>(
    cwd: &Path,
    args: I,
    fake_bin: &Path,
    fake_gh_dir: &Path,
    env: &[(&str, &str)],
) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", fake_bin.display(), old_path.to_string_lossy());
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("PATH", path)
        .env("GH_FAKE_DIR", fake_gh_dir);
    for (key, value) in env {
        command.env(key, value);
    }
    run(command)
}

fn knit_fails_with_fake_gh<I, S>(cwd: &Path, args: I, fake_bin: &Path, fake_gh_dir: &Path) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    knit_fails_with_fake_gh_env(cwd, args, fake_bin, fake_gh_dir, &[])
}

fn knit_fails_with_fake_gh_env<I, S>(
    cwd: &Path,
    args: I,
    fake_bin: &Path,
    fake_gh_dir: &Path,
    env: &[(&str, &str)],
) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", fake_bin.display(), old_path.to_string_lossy());
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("PATH", path)
        .env("GH_FAKE_DIR", fake_gh_dir);
    for (key, value) in env {
        command.env(key, value);
    }
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

#[cfg(unix)]
fn write_fake_gh(fake_bin: &Path, fake_gh_dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(fake_bin).unwrap();
    fs::create_dir_all(fake_gh_dir).unwrap();
    let script = fake_bin.join("gh");
    fs::write(
        &script,
        r#"#!/bin/sh
set -eu

if [ "$1" != "pr" ]; then
  echo "unexpected gh command: $*" >&2
  exit 1
fi
shift
sub="$1"
shift
repo="$(basename "$PWD")"

case "$sub" in
  list)
    printf '[]\n'
    ;;
  create)
    cat > "$GH_FAKE_DIR/create-$repo.md"
    case "$repo" in
      backend) number=101 ;;
      frontend) number=202 ;;
      *) number=303 ;;
    esac
    printf 'https://github.com/acme/%s/pull/%s\n' "$repo" "$number"
    ;;
  view)
    url="$1"
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    number="${url##*/}"
    state="OPEN"
    if [ -f "$GH_FAKE_DIR/merged-$pr_repo" ]; then
      state="MERGED"
    fi
    draft="false"
    if [ "${GH_FAKE_DRAFT:-0}" = "1" ]; then
      draft="true"
    fi
    printf '{"number":%s,"url":"%s","state":"%s","title":"%s PR","baseRefName":"main","headRefName":"knit/venue-capacity","body":"Existing body","isDraft":%s,"headRefOid":"%s-head"}\n' "$number" "$url" "$state" "$pr_repo" "$draft" "$pr_repo"
    ;;
  edit)
    url="$1"
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    cat > "$GH_FAKE_DIR/edit-$pr_repo.md"
    printf '%s\n' "$url"
    ;;
  checks)
    if [ "${GH_FAKE_CHECKS_FAIL:-0}" = "1" ]; then
      printf '[{"name":"test","state":"FAILURE","bucket":"fail"}]\n'
    else
      printf '[]\n'
    fi
    ;;
  merge)
    url="$1"
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    printf '%s\n' "$pr_repo" >> "$GH_FAKE_DIR/merge-order.txt"
    touch "$GH_FAKE_DIR/merged-$pr_repo"
    printf 'Merged pull request %s\n' "$url"
    ;;
  *)
    echo "unexpected gh pr command: $sub" >&2
    exit 1
    ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
}

#[cfg(not(unix))]
fn write_fake_gh(_fake_bin: &Path, _fake_gh_dir: &Path) {
    panic!("fake gh smoke test requires a unix-like shell");
}
