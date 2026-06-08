use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn init_can_generate_agents_tutorial() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let output = knit(&workspace, ["bundle", "start", "venue capacity", "--agents"]);
    assert!(output.contains("AGENTS.md"));

    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("This is a Knit workspace"));
    assert!(agents.contains("knit status"));
    assert!(agents.contains("knit project init"));
    assert!(agents.contains("knit bundle start"));
    assert!(agents.contains("knit bundle add"));
    assert!(agents.contains("knit bundle remove <repo>"));
    assert!(agents.contains("knit bundle prune"));
    assert!(agents.contains("knit bundle prune --apply --worktrees --branches"));
    assert!(agents.contains("knit bundle prune --apply --all"));
    assert!(agents.contains("--remote-branches"));
    assert!(agents.contains("matching KnitHub remote bundle records"));
    assert!(agents.contains("requires a token with `bundle:delete`"));
    assert!(agents.contains("knit project remove <project> --force"));
    assert!(agents.contains("knit --bundle feature-a commit"));
    assert!(agents.contains("knit --bundle feature-a commit --stage"));
    assert!(agents.contains("knit --bundle feature-a push --set-upstream"));
    assert!(agents.contains("Project JSON can define a default `landing` template"));
    assert!(agents.contains(".knit/land-plans/<bundle>.land.json"));
    assert!(agents.contains("gloss prepare"));

    fs::remove_file(workspace.join("AGENTS.md")).unwrap();
    let existing_workspace_output = knit(&workspace, ["bundle", "start", "venue capacity", "--agents"]);
    assert!(existing_workspace_output.contains("AGENTS.md"));
    let existing_workspace_agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(existing_workspace_agents.contains("This is a Knit workspace"));

    fs::write(workspace.join("AGENTS.md"), "custom guidance\n").unwrap();
    knit(
        &workspace,
        ["bundle", "start", "venue capacity", "--force", "--agents"],
    );
    let updated = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(updated.contains("custom guidance"));
    assert!(updated.contains("This is a Knit workspace"));
    assert_eq!(updated.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);

    knit(
        &workspace,
        ["bundle", "start", "venue capacity", "--force", "--agents"],
    );
    let rerun = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert_eq!(rerun.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_default_repos_start_bundle_without_track() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let scraper = root.join("scraper");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    init_repo(&scraper, "scraper");

    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );
    knit(
        &workspace,
        [
            "project",
            "add",
            "scraper",
            scraper.to_str().unwrap(),
            "--observe",
        ],
    );
    assert!(knit(&workspace, ["project", "list"]).contains("arbient"));
    assert!(knit(&workspace, ["project", "show"]).contains("\"id\": \"arbient\""));

    knit(&workspace, ["bundle", "start", "project feature"]);

    assert!(workspace
        .join(".knit/worktrees/project-feature/backend")
        .exists());
    assert!(workspace
        .join(".knit/worktrees/project-feature/frontend")
        .exists());
    assert!(!workspace
        .join(".knit/worktrees/project-feature/scraper")
        .exists());

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/project-feature.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bundle["projectId"].as_str(), Some("arbient"));
    assert_eq!(bundle["repos"].as_array().unwrap().len(), 2);
    assert_eq!(bundle["repos"][0]["id"].as_str(), Some("backend"));
    assert_eq!(bundle["repos"][1]["id"].as_str(), Some("frontend"));

    let list = knit(&workspace, ["bundle", "list"]);
    assert!(list.contains("project-feature"));

    fs::remove_dir_all(root).unwrap();
}

fn setup_three_repo_project(workspace: &Path, root: &Path) {
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let docs = root.join("docs");
    fs::create_dir_all(workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    init_repo(&docs, "docs");
    knit(workspace, ["project", "init", "arbient"]);
    knit(
        workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );
    knit(
        workspace,
        [
            "project",
            "add",
            "docs",
            docs.to_str().unwrap(),
            "--observe",
        ],
    );
}

fn bundle_repo_ids(workspace: &Path, bundle_id: &str) -> Vec<String> {
    let path = workspace
        .join(".knit/bundles")
        .join(format!("{bundle_id}.bundle.json"));
    let bundle: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    bundle["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn views_apply_default_and_named_shapes_on_bundle_start() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(
        &workspace,
        ["view", "save", "backend", "--exclude", "frontend"],
    );
    knit(
        &workspace,
        [
            "view",
            "save",
            "frontend",
            "--include",
            "docs",
            "--exclude",
            "backend",
        ],
    );
    knit(&workspace, ["view", "default", "backend"]);

    let list = knit(&workspace, ["view", "list"]);
    assert!(list.contains("backend"), "{list}");
    assert!(list.contains("frontend"), "{list}");

    // Default view (backend) drops frontend; docs is observed, so backend only.
    knit(&workspace, ["bundle", "start", "default feature"]);
    assert_eq!(
        bundle_repo_ids(&workspace, "default-feature"),
        vec!["backend"]
    );

    // Named view "frontend" => {frontend, docs}; ad-hoc --include adds backend.
    knit(
        &workspace,
        [
            "bundle",
            "start",
            "named feature",
            "--view",
            "frontend",
            "--include",
            "backend",
        ],
    );
    let ids = bundle_repo_ids(&workspace, "named-feature");
    assert_eq!(ids, vec!["backend", "frontend", "docs"], "{ids:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn view_flag_conflicts_with_repo_selection() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    knit(
        &workspace,
        ["view", "save", "backend", "--exclude", "frontend"],
    );

    let error = knit_fails(
        &workspace,
        [
            "bundle", "start", "x", "--view", "backend", "--repo", "backend",
        ],
    );
    assert!(error.contains("not together with --repo"), "{error}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_include_and_exclude_manage_worktrees() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(&workspace, ["bundle", "start", "live feature"]);
    let worktrees = workspace.join(".knit/worktrees/live-feature");
    assert!(worktrees.join("backend").exists());
    assert!(worktrees.join("frontend").exists());
    assert!(!worktrees.join("docs").exists());

    // Include the observed repo: its worktree is materialized.
    knit(&workspace, ["bundle", "add", "docs"]);
    assert!(worktrees.join("docs").exists());
    assert!(bundle_repo_ids(&workspace, "live-feature").contains(&"docs".to_string()));

    // Exclude (default): worktree removed, feature branch kept.
    knit(&workspace, ["bundle", "remove", "frontend"]);
    assert!(!worktrees.join("frontend").exists());
    assert!(!bundle_repo_ids(&workspace, "live-feature").contains(&"frontend".to_string()));
    assert!(
        git(
            &root.join("frontend"),
            ["branch", "--list", "knit/live-feature"]
        )
        .contains("knit/live-feature"),
        "feature branch should be preserved by default"
    );

    // Exclude with --delete-branch: worktree removed and branch deleted.
    knit(&workspace, ["bundle", "remove", "docs", "--delete-branch"]);
    assert!(!worktrees.join("docs").exists());
    assert!(
        !git(
            &root.join("docs"),
            ["branch", "--list", "knit/live-feature"]
        )
        .contains("knit/live-feature"),
        "feature branch should be deleted with --delete-branch"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn bundle_start_cd_opens_project_worktree_root() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    let output = knit_with_env(
        &workspace,
        ["bundle", "start", "project feature", "--agents", "--cd"],
        &[("SHELL", "/bin/pwd")],
    );
    let checkout = workspace
        .join(".knit/worktrees/project-feature")
        .canonicalize()
        .unwrap();
    assert!(checkout.join("backend").exists());
    assert!(checkout.join("frontend").exists());
    assert!(output.contains("Bundle AGENTS.md:"));
    let bundle_agents = fs::read_to_string(checkout.join("AGENTS.md")).unwrap();
    assert!(bundle_agents.contains("Knit Bundle Worktree Guide"));
    assert!(bundle_agents.contains("bundle `project-feature`"));
    assert!(bundle_agents.contains("knit status"));
    assert!(bundle_agents.contains("`backend`: `.knit/worktrees/project-feature/backend`"));
    assert!(bundle_agents.contains("`frontend`: `.knit/worktrees/project-feature/frontend`"));
    assert!(output.contains("cd:"));
    assert!(
        output
            .lines()
            .any(|line| line.trim() == checkout.to_str().unwrap()),
        "{output}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn bundle_start_cd_accepts_repo_selector() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    let output = knit_with_env(
        &workspace,
        ["bundle", "start", "project feature", "--cd", "frontend"],
        &[("SHELL", "/bin/pwd")],
    );
    let checkout = workspace
        .join(".knit/worktrees/project-feature/frontend")
        .canonicalize()
        .unwrap();
    assert!(
        output
            .lines()
            .any(|line| line.trim() == checkout.to_str().unwrap()),
        "{output}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn work_item_start_links_bundle_and_writes_prompt() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(&workspace, ["org", "init", "acme"]);
    knit(&workspace, ["project", "set-org", "acme"]);
    knit(
        &workspace,
        [
            "work-item",
            "add",
            "Dispatch approved work",
            "--kind",
            "feature",
            "--description",
            "Create the worktree and prompt.",
            "--repo",
            "backend",
            "--accept",
            "A bundle is linked",
        ],
    );
    knit(
        &workspace,
        ["work-item", "approve", "dispatch-approved-work"],
    );
    knit(&workspace, ["work-item", "start", "dispatch-approved-work"]);

    let item: Value = serde_json::from_str(
        &fs::read_to_string(
            workspace.join(".knit/work-items/dispatch-approved-work.work-item.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(item["planningStatus"].as_str(), Some("approved"));
    assert_eq!(item["executionStatus"].as_str(), Some("claimed"));
    assert_eq!(
        item["bundleIds"][0].as_str(),
        Some("dispatch-approved-work")
    );

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/dispatch-approved-work.bundle.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        bundle["workItemIds"][0].as_str(),
        Some("dispatch-approved-work")
    );
    assert!(workspace
        .join(".knit/worktrees/dispatch-approved-work/WORK_ITEM.md")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_remove_deletes_template_and_clears_active_project() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    let project_path = workspace.join(".knit/projects/arbient.project.json");
    assert!(project_path.exists());
    let refused = knit_fails(&workspace, ["project", "remove", "arbient"]);
    assert!(refused.contains("requires --force"));

    let removed = knit(&workspace, ["project", "remove", "arbient", "--force"]);
    assert!(removed.contains("Removed project"));
    assert!(!project_path.exists());
    let config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    assert!(config.get("activeProject").is_none());
    assert!(!knit(&workspace, ["project", "list"]).contains("arbient"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_split_cherrypicks_source_commits_into_a_new_bundle() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    knit(
        &workspace,
        ["bundle", "start", "source feature", "--repo", "backend"],
    );
    let source_checkout = workspace.join(".knit/worktrees/source-feature/backend");
    append_line(&source_checkout.join("app.txt"), "source change");
    git(&source_checkout, ["add", "app.txt"]);
    git(&source_checkout, ["commit", "-m", "Source change"]);
    let source_sha = git(&source_checkout, ["rev-parse", "HEAD"]);
    knit(&workspace, ["--bundle", "source-feature", "sync"]);

    let split = knit(
        &workspace,
        [
            "bundle",
            "split",
            "source-feature",
            "--title",
            "picked feature",
            source_sha.trim(),
        ],
    );
    assert!(split.contains("picking"));

    let picked_checkout = workspace.join(".knit/worktrees/picked-feature/backend");
    assert!(fs::read_to_string(picked_checkout.join("app.txt"))
        .unwrap()
        .contains("source change"));
    assert!(git(&picked_checkout, ["log", "-1", "--pretty=%B"]).contains("Source change"));

    let picked_bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/picked-feature.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(picked_bundle["repos"][0]["id"].as_str(), Some("backend"));
    assert!(picked_bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["type"].as_str() == Some("git.observed")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_split_preflights_project_repos_before_creating_bundle() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    knit(
        &workspace,
        ["bundle", "start", "source feature", "--repo", "backend"],
    );
    knit(
        &workspace,
        [
            "--bundle",
            "source-feature",
            "bundle",
            "add",
            frontend.to_str().unwrap(),
        ],
    );
    let frontend_checkout = workspace.join(".knit/worktrees/source-feature/frontend");
    append_line(&frontend_checkout.join("app.txt"), "frontend source change");
    git(&frontend_checkout, ["add", "app.txt"]);
    git(
        &frontend_checkout,
        ["commit", "-m", "Frontend source change"],
    );
    let frontend_sha = git(&frontend_checkout, ["rev-parse", "HEAD"]);
    knit(&workspace, ["--bundle", "source-feature", "sync"]);

    let split = knit_fails(
        &workspace,
        [
            "bundle",
            "split",
            "source-feature",
            "--title",
            "bad split",
            frontend_sha.trim(),
        ],
    );
    assert!(split.contains("Project arbient is missing repo(s) needed for this split: frontend"));
    assert!(!workspace
        .join(".knit/bundles/bad-split.bundle.json")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_agents_are_generated_from_project_json() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    fs::write(
        workspace.join("AGENTS.md"),
        "custom guidance\n\n## Arbient Knit Project\n\nThat command should add all four Arbient repos by default:\n\n- `backend`\n\n<!-- BEGIN GLOSS AGENTS -->\nkeep this\n<!-- END GLOSS AGENTS -->\n",
    )
    .unwrap();

    let refresh = knit(&workspace, ["project", "init", "arbient", "--agents"]);
    assert!(refresh.contains("Project AGENTS.md"));
    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("custom guidance"));
    assert!(agents.contains("<!-- BEGIN KNIT PROJECT AGENTS: arbient -->"));
    assert!(agents.contains("That command adds these default repos from the project data:"));
    assert!(agents.contains("- `backend`"));
    assert!(!agents.contains("all four Arbient repos"));
    assert!(agents.contains("<!-- BEGIN GLOSS AGENTS -->"));
    assert!(
        agents.contains("<!-- END KNIT PROJECT AGENTS: arbient -->\n<!-- BEGIN GLOSS AGENTS -->")
    );

    knit(
        &workspace,
        [
            "project",
            "add",
            "frontend",
            frontend.to_str().unwrap(),
            "--agents",
        ],
    );
    let updated = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert_eq!(
        updated
            .matches("<!-- BEGIN KNIT PROJECT AGENTS: arbient -->")
            .count(),
        1
    );
    assert!(updated.contains("- `backend`"));
    assert!(updated.contains("- `frontend`"));

    let project_path = workspace.join(".knit/projects/arbient.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "merge": {
            "repoOrder": ["backend", "frontend"],
            "method": "squash",
            "requiredChecksOnly": true
        },
        "deployments": [
            {
                "id": "deploy-backend",
                "repoId": "backend",
                "checkout": { "branch": "main", "remote": "origin", "update": "pull" },
                "command": ["fly", "deploy"]
            },
            {
                "id": "deploy-frontend",
                "repoId": "frontend",
                "mode": "push"
            }
        ]
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["project", "agents", "arbient"]);
    let landing_agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(landing_agents.contains("This project defines a default landing template"));
    assert!(landing_agents
        .contains("`knit land` expands it into `.knit/land-plans/<bundle>.land.json`"));
    assert!(landing_agents.contains("- `backend`"));
    assert!(landing_agents.contains("- `frontend`"));
    assert!(
        landing_agents.contains("Merge defaults: method `squash`, required checks only `true`.")
    );
    assert!(landing_agents.contains("`deploy-backend` repo `backend` uses `command` from `origin/main` with `pull`: `fly deploy`"));
    assert!(landing_agents.contains("`deploy-frontend` repo `frontend` uses `push`"));
    assert!(landing_agents.contains("Do not use `gh pr merge` for Knit-owned bundles."));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn worktree_agents_are_written_by_default_and_refreshed_with_agents_flag() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    let start = knit(&workspace, ["bundle", "start", "agent docs"]);
    assert!(start.contains("Worktree AGENTS.md: 2 repo worktree(s)"));
    assert!(!workspace.join("AGENTS.md").exists());

    let bundle_agents_path = workspace.join(".knit/worktrees/agent-docs/AGENTS.md");
    let backend_agents_path = workspace.join(".knit/worktrees/agent-docs/backend/AGENTS.md");
    let frontend_agents_path = workspace.join(".knit/worktrees/agent-docs/frontend/AGENTS.md");
    let bundle_agents = fs::read_to_string(&bundle_agents_path).unwrap();
    assert!(bundle_agents.contains("Knit Bundle Worktree Guide"));
    assert!(bundle_agents.contains("bundle `agent-docs`"));
    assert!(bundle_agents.contains("`backend`: `.knit/worktrees/agent-docs/backend`"));
    assert!(bundle_agents.contains("`frontend`: `.knit/worktrees/agent-docs/frontend`"));
    let backend_agents = fs::read_to_string(&backend_agents_path).unwrap();
    assert!(backend_agents.contains("Knit Worktree Guide"));
    assert!(backend_agents.contains("bundle `agent-docs`"));
    assert!(backend_agents.contains("repo `backend`"));
    assert!(backend_agents.contains("knit commit --stage"));
    assert!(backend_agents.contains("knit push --set-upstream"));
    assert!(!backend_agents.contains("knit --bundle"));
    assert!(backend_agents.contains("`frontend`: `.knit/worktrees/agent-docs/frontend`"));
    assert!(frontend_agents_path.exists());
    assert!(git(
        &workspace.join(".knit/worktrees/agent-docs/backend"),
        ["status", "--short"]
    )
    .trim()
    .is_empty());

    fs::write(&backend_agents_path, "repo guidance\n").unwrap();
    knit(&workspace, ["bundle", "start", "agent docs", "--agents"]);
    let updated = fs::read_to_string(&backend_agents_path).unwrap();
    assert!(updated.contains("repo guidance"));
    assert_eq!(updated.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);
    assert!(updated.contains("knit status"));
    assert!(updated.contains("knit push --set-upstream"));
    assert!(!updated.contains("knit --bundle"));
    assert!(workspace.join("AGENTS.md").exists());

    let second_start = knit(&workspace, ["bundle", "start", "agent docs two"]);
    assert!(second_start.contains("Worktree AGENTS.md: 2 repo worktree(s)"));
    let second_agents =
        fs::read_to_string(workspace.join(".knit/worktrees/agent-docs-two/backend/AGENTS.md"))
            .unwrap();
    assert!(second_agents.contains("bundle `agent-docs-two`"));
    assert!(second_agents.contains("knit commit --stage"));
    assert!(second_agents.contains("knit push --set-upstream"));
    assert!(!second_agents.contains("knit --bundle"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_start_and_add_support_ad_hoc_work() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "start", "ad hoc"]);
    assert!(knit(&workspace, ["bundle"]).contains("Bundle: ad-hoc"));
    let empty_bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/ad-hoc.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(empty_bundle["repos"].as_array().unwrap().len(), 0);

    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    assert!(workspace.join(".knit/worktrees/ad-hoc/backend").exists());
    let worktree_agents =
        fs::read_to_string(workspace.join(".knit/worktrees/ad-hoc/backend/AGENTS.md")).unwrap();
    assert!(worktree_agents.contains("bundle `ad-hoc`"));
    assert!(!worktree_agents.contains("knit --bundle"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_does_not_duplicate_ledger_commits_when_head_projection_is_stale() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["bundle", "start", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    fs::write(
        workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "feature\n",
    )
    .unwrap();
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Add venue capacity integration"],
    );

    let bundle_path = workspace.join(".knit/bundles/venue-capacity.bundle.json");
    let mut bundle = read_bundle(&workspace);
    let base_sha = bundle["repos"][0]["baseSha"].as_str().unwrap().to_string();
    let group_sha = bundle["commitGroups"][0]["commits"][0]["sha"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        bundle["repos"][0]["headSha"].as_str(),
        Some(group_sha.as_str())
    );

    bundle["repos"][0]["headSha"] = json!(base_sha);
    fs::write(
        &bundle_path,
        format!("{}\n", serde_json::to_string_pretty(&bundle).unwrap()),
    )
    .unwrap();

    let status = knit(&workspace, ["status"]);
    assert!(!status.contains("unrecorded commits"));

    let sync = knit(&workspace, ["sync"]);
    assert!(sync.contains("No unrecorded git commits found."));
    let log = knit(&workspace, ["log"]);
    assert!(log.contains("Add venue capacity integration"));
    assert!(!log.contains("observed git changes"));

    let doctor = knit_fails(&workspace, ["doctor"]);
    assert!(doctor.contains("headSha projection differs from ledger"));
    let migrate_check = knit_fails(&workspace, ["migrate", "--check"]);
    assert!(migrate_check.contains("need migration"));
    knit(&workspace, ["migrate"]);
    let repaired = read_bundle(&workspace);
    assert_eq!(
        repaired["repos"][0]["headSha"].as_str(),
        Some(group_sha.as_str())
    );
    assert!(knit(&workspace, ["doctor"]).contains("Knit doctor: ok"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_context_supports_parallel_worktrees_and_folder_switches() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    let subdir = workspace.join("subdir");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&subdir).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "start", "fix a"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    knit(&workspace, ["bundle", "start", "fix b"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    let fix_a = workspace.join(".knit/worktrees/fix-a/backend");
    let fix_b = workspace.join(".knit/worktrees/fix-b/backend");
    assert!(fix_a.exists());
    assert!(fix_b.exists());
    assert_eq!(
        git(&fix_a, ["branch", "--show-current"]).trim(),
        "knit/fix-a"
    );
    assert_eq!(
        git(&fix_b, ["branch", "--show-current"]).trim(),
        "knit/fix-b"
    );

    let fix_a_status = knit(&fix_a, ["status"]);
    assert!(fix_a_status.contains("Bundle: fix-a (cwd)"));
    let fix_b_status = knit(&fix_b, ["status"]);
    assert!(fix_b_status.contains("Bundle: fix-b (cwd)"));

    assert!(knit(&workspace, ["--bundle", "fix-b", "status"]).contains("Bundle: fix-b (explicit)"));
    assert!(
        knit_with_env(&workspace, ["status"], &[("KNIT_BUNDLE", "fix-b")])
            .contains("Bundle: fix-b (env)")
    );

    assert!(knit_fails(&workspace, ["switch", "fix-a"]).contains("without --workspace"));
    knit(&workspace, ["switch", "fix-a", "--workspace"]);
    assert!(knit_fails(&workspace, ["status"]).contains("multiple open bundles"));
    assert!(knit(&workspace, ["--bundle", "fix-a", "status"]).contains("Bundle: fix-a (explicit)"));

    knit(&subdir, ["switch", "fix-b"]);
    assert!(knit(&subdir, ["status"]).contains("Bundle: fix-b (folder)"));
    assert!(knit_fails(&workspace, ["status"]).contains("multiple open bundles"));

    knit(&fix_a, ["switch", "fix-b", "--here"]);
    assert!(knit(&fix_a, ["status"]).contains("Bundle: fix-a (cwd)"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn commit_from_worktree_uses_worktree_bundle_not_workspace_fallback() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "start", "test"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    knit(&workspace, ["bundle", "start", "test2"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    assert!(knit_fails(&workspace, ["status"]).contains("multiple open bundles"));

    let test_checkout = workspace.join(".knit/worktrees/test/backend");
    fs::write(test_checkout.join("test.md"), "test\n").unwrap();
    assert!(
        knit_fails(&workspace, ["commit", "--stage", "-m", "Add test"])
            .contains("multiple open bundles")
    );

    let commit = knit(&test_checkout, ["commit", "--stage", "-m", "Add test"]);
    assert!(commit.contains("Recorded commit group"));

    let test_bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/test.bundle.json")).unwrap(),
    )
    .unwrap();
    let test2_bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/test2.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(test_bundle["commitGroups"].as_array().unwrap().len(), 1);
    assert_eq!(test2_bundle["commitGroups"].as_array().unwrap().len(), 0);
    assert!(test_checkout.join("test.md").exists());
    assert!(!workspace
        .join(".knit/worktrees/test2/backend/test.md")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reset_hard_restores_tracked_changes_in_project_source_repos() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["project", "init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    // Dirty the canonical source checkout the way an external tool would.
    fs::write(backend.join("app.txt"), "tampered\n").unwrap();
    fs::write(backend.join("untracked.txt"), "new\n").unwrap();

    let output = knit(&workspace, ["reset", "--hard", "--all"]);
    assert!(output.contains("resetting source checkouts for project demo"));

    // Tracked change is discarded; the untracked file survives, mirroring `git reset --hard`.
    assert_eq!(
        fs::read_to_string(backend.join("app.txt")).unwrap(),
        "backend\n"
    );
    assert!(backend.join("untracked.txt").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reset_hard_resets_bundle_worktree_when_resolved_from_worktree() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "start", "feature"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    let checkout = workspace.join(".knit/worktrees/feature/backend");
    fs::write(checkout.join("app.txt"), "tampered\n").unwrap();

    // Resolved from the worktree cwd, reset targets the bundle checkout, not project sources.
    let output = knit(&checkout, ["reset", "--hard"]);
    assert!(!output.contains("resetting source checkouts"));
    assert_eq!(
        fs::read_to_string(checkout.join("app.txt")).unwrap(),
        "backend\n"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn run_executes_named_project_commands_in_bundle_worktrees() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        [
            "project",
            "command",
            "set",
            "show-root",
            "--repo",
            "backend",
            "--",
            "git",
            "rev-parse",
            "--show-toplevel",
        ],
    );
    assert!(knit(&workspace, ["project", "command", "list"]).contains("show-root"));

    knit(&workspace, ["bundle", "start", "run feature"]);
    let worktree = workspace.join(".knit/worktrees/run-feature/backend");
    let named = knit(&workspace, ["run", "show-root"]);
    assert!(named.contains(worktree.to_str().unwrap()));

    let raw = knit(
        &workspace,
        [
            "run",
            "--repo",
            "backend",
            "--",
            "git",
            "branch",
            "--show-current",
        ],
    );
    assert_eq!(raw.trim(), "knit/run-feature");
    assert!(knit(&workspace, ["run", "--list"]).contains("show-root"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn merge_bundle_into_branch_rolls_back_on_conflict_by_default() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    git(&backend, ["branch", "staging"]);

    knit(&workspace, ["bundle", "start", "feature x"]);
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
            "--stage",
            "-m",
            "Feature X",
        ],
    );

    knit(&workspace, ["bundle", "start", "feature y"]);
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
            "--stage",
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
fn merge_manual_conflict_can_continue_and_compat_bundle_can_target_bundle() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    git(&backend, ["branch", "staging"]);

    knit(&workspace, ["bundle", "start", "feature x"]);
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
            "--stage",
            "-m",
            "Feature X",
        ],
    );

    knit(&workspace, ["bundle", "start", "feature y"]);
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
            "--stage",
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

    knit(
        &workspace,
        [
            "bundle",
            "compat",
            "feature-x",
            "feature-y",
            "--title",
            "x y compat",
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

    knit(&workspace, ["bundle", "start", "merge push"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    fs::write(
        workspace.join(".knit/worktrees/merge-push/backend/feature.txt"),
        "feature merge push\n",
    )
    .unwrap();
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Feature merge push"],
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

#[test]
fn bundle_lifecycle_clean_schema_migrate_doctor_and_advice_work() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "start", "life cycle"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    assert!(knit(&workspace, ["schema", "print", "bundle"]).contains("ChangeGroup"));

    knit(&workspace, ["bundle", "close", "--reason", "done"]);
    let closed_bundle_path = workspace.join(".knit/bundles/life-cycle.bundle.json");
    let closed_bundle: Value =
        serde_json::from_str(&fs::read_to_string(&closed_bundle_path).unwrap()).unwrap();
    assert_eq!(closed_bundle["state"].as_str(), Some("closed"));

    knit(&workspace, ["clean", "--closed", "--worktrees"]);
    assert!(!workspace
        .join(".knit/worktrees/life-cycle/backend")
        .exists());

    knit(&workspace, ["bundle", "archive", "life-cycle"]);
    assert!(!knit(&workspace, ["bundle", "list"]).contains("life-cycle"));
    assert!(knit(&workspace, ["bundle", "list", "--archived"]).contains("archived"));
    assert!(knit_fails(&workspace, ["switch", "life-cycle"]).contains("archived"));
    knit(&workspace, ["bundle", "restore", "life-cycle"]);
    assert!(knit(&workspace, ["bundle", "list"]).contains("closed"));

    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    config.as_object_mut().unwrap().remove("advice");
    fs::write(
        workspace.join(".knit/config.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&closed_bundle_path).unwrap()).unwrap();
    bundle.as_object_mut().unwrap().remove("state");
    fs::write(
        &closed_bundle_path,
        serde_json::to_string_pretty(&bundle).unwrap(),
    )
    .unwrap();
    assert!(knit_fails(&workspace, ["migrate", "--check"]).contains("need migration"));
    knit(&workspace, ["migrate"]);
    assert!(fs::read_to_string(workspace.join(".knit/config.json"))
        .unwrap()
        .contains("\"advice\": true"));
    assert!(fs::read_to_string(&closed_bundle_path)
        .unwrap()
        .contains("\"state\": \"closed\""));

    assert!(knit(&workspace, ["doctor"]).contains("Knit doctor: ok"));
    fs::write(workspace.join(".knit/locks/stale.lock"), "").unwrap();
    assert!(knit_fails(&workspace, ["doctor"]).contains("stale lock"));
    fs::remove_file(workspace.join(".knit/locks/stale.lock")).unwrap();

    knit(&workspace, ["config", "set", "advice", "false"]);
    assert!(fs::read_to_string(workspace.join(".knit/config.json"))
        .unwrap()
        .contains("\"advice\": false"));

    knit(&workspace, ["bundle", "delete", "life-cycle", "--force"]);
    assert!(!closed_bundle_path.exists());
    assert!(workspace
        .join(".knit/deleted/bundles/life-cycle.bundle.json")
        .exists());
    assert!(knit(&workspace, ["bundle", "list", "--deleted"]).contains("deleted"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_config_supports_global_fallback_and_workspace_override() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    let knit_home = root.join("knit-home");
    fs::create_dir_all(&workspace).unwrap();
    let env = [("KNIT_HOME", knit_home.to_str().unwrap())];

    knit_with_env(
        &workspace,
        [
            "remote",
            "add",
            "--global",
            "knithub",
            "https://api.knithub.dev",
            "--token",
            "global-token",
        ],
        &env,
    );
    let global_config: Value =
        serde_json::from_str(&fs::read_to_string(knit_home.join("config.json")).unwrap()).unwrap();
    assert_eq!(
        global_config["remotes"]["knithub"]["url"].as_str(),
        Some("https://api.knithub.dev")
    );

    knit_with_env(&workspace, ["project", "init", "arbient"], &env);
    let inherited = knit_with_env(&workspace, ["remote", "show", "knithub"], &env);
    assert!(inherited.contains("https://api.knithub.dev"));
    assert!(inherited.contains("Scope: global"));
    assert!(inherited.contains("Token: stored"));

    knit_with_env(
        &workspace,
        ["remote", "add", "knithub", "http://localhost:4000"],
        &env,
    );
    let overridden = knit_with_env(&workspace, ["remote", "show", "knithub"], &env);
    assert!(overridden.contains("http://localhost:4000"));
    assert!(overridden.contains("Scope: workspace"));

    let global_only = knit_with_env(&workspace, ["remote", "show", "--global", "knithub"], &env);
    assert!(global_only.contains("https://api.knithub.dev"));
    assert!(global_only.contains("Scope: global"));

    let show = knit_with_env(&workspace, ["config", "show"], &env);
    assert!(show.contains("Global config"));
    assert!(show.contains("Effective config"));
    assert!(show.contains("https://api.knithub.dev"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn config_can_target_multiple_knithub_sync_remotes() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "remote fanout"]);
    knit(
        &workspace,
        ["remote", "add", "local", "http://localhost:4000"],
    );
    knit(&workspace, ["remote", "add", "prod", "https://knithub.dev"]);

    let set = knit(&workspace, ["config", "set", "sync-remotes", "local,prod"]);
    assert!(set.contains("sync-remotes=local,prod"), "{set}");
    let config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    assert_eq!(config["syncRemote"].as_str(), Some("local"));
    assert_eq!(
        config["syncRemotes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["local", "prod"]
    );

    let list = knit(&workspace, ["remote", "list"]);
    assert!(list.contains("local"), "{list}");
    assert!(list.contains("prod"), "{list}");
    assert_eq!(list.matches("sync").count(), 2, "{list}");

    let missing = knit_fails(&workspace, ["config", "set", "sync-remotes", "missing"]);
    assert!(
        missing.contains("No KnitHub remote named `missing`"),
        "{missing}"
    );

    knit(&workspace, ["remote", "remove", "local"]);
    let config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    assert_eq!(config["syncRemote"].as_str(), Some("prod"));
    assert_eq!(
        config["syncRemotes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["prod"]
    );

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

    knit(&workspace, ["bundle", "venue capacity"]);
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
            "bundle",
            "add",
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
    assert!(diff_stat.contains("Bundle: venue-capacity (workspace)"));
    assert!(diff_stat.contains("== backend"));
    assert!(diff_stat.contains("== frontend"));
    assert!(diff_stat.contains("== scraper"));
    assert!(diff_stat.contains("app.txt"));

    let frontend_diff = knit(&workspace, ["diff", "frontend"]);
    assert!(frontend_diff.contains("Bundle: venue-capacity (workspace)"));
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

    knit(&workspace, ["bundle", "remove", "--keep-worktree", "scraper"]);

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

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
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

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
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
fn pull_main_fast_forwards_project_repos_and_reports() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, backend_collab) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collab) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["project", "init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    // Advance backend's origin main; leave frontend with a local dirty edit.
    append_line(&backend_collab.join("app.txt"), "remote main update");
    git(&backend_collab, ["add", "app.txt"]);
    git(&backend_collab, ["commit", "-m", "Remote main update"]);
    git(&backend_collab, ["push", "origin", "main"]);
    let backend_sha = git(&backend_collab, ["rev-parse", "HEAD"]);
    append_line(&frontend.join("app.txt"), "local uncommitted edit");

    let report = knit(&workspace, ["pull", "--main"]);
    assert!(report.contains("Main repos:"));
    assert!(report.contains("backend"));
    assert!(report.contains(&backend_sha[..7]));
    assert!(report.contains("frontend"));
    assert!(report.contains("skipped"));

    // Backend's source checkout fast-forwarded; the dirty repo was left alone.
    assert_eq!(git(&backend, ["rev-parse", "HEAD"]), backend_sha);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_everything_at_root_reports_without_refusing_multiple_bundles() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collab) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["project", "init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    // Two open bundles: the old workspace-fallback guard refused a bare pull at
    // the root. The new aggregate pull reports instead.
    knit(&workspace, ["bundle", "start", "feature one"]);
    knit(&workspace, ["bundle", "start", "feature two"]);

    let report = knit(&workspace, ["pull"]);
    assert!(!report.contains("Refusing"));
    assert!(report.contains("Main repos:"));
    assert!(report.contains("Bundles:"));
    assert!(report.contains("feature-one"));
    assert!(report.contains("feature-two"));
    assert!(report.contains("Pulled:"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_bundles_without_remote_reports_each_bundle_skipped() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "start", "feature one"]);
    knit(&workspace, ["bundle", "start", "feature two"]);

    let report = knit(&workspace, ["pull", "--bundles"]);
    assert!(report.contains("Bundles:"));
    assert!(report.contains("feature-one"));
    assert!(report.contains("feature-two"));
    assert!(report.contains("no KnitHub remote configured"));
    // --bundles alone does not touch project main repos.
    assert!(!report.contains("Main repos:"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn fetch_updates_remote_refs_without_moving_checkout_or_bundle_base() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
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

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
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
#[cfg(unix)]
fn push_sends_selected_feature_branches_in_parallel() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    let backend_feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    let frontend_feature = workspace.join(".knit/worktrees/venue-capacity/frontend");

    append_line(&backend_feature.join("app.txt"), "parallel backend push");
    append_line(&frontend_feature.join("app.txt"), "parallel frontend push");
    knit(&workspace, ["commit", "--stage", "-m", "Parallel push"]);
    let backend_sha = git(&backend_feature, ["rev-parse", "HEAD"]);
    let frontend_sha = git(&frontend_feature, ["rev-parse", "HEAD"]);

    let gate = root.join("push-gate");
    install_parallel_push_hook(&backend_feature, &gate, "backend", "frontend");
    install_parallel_push_hook(&frontend_feature, &gate, "frontend", "backend");

    let push = knit(&workspace, ["push", "backend", "frontend"]);
    assert!(push.contains("backend"));
    assert!(push.contains("frontend"));
    assert_eq!(
        git(
            &backend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        backend_sha
    );
    assert_eq!(
        git(
            &frontend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        frontend_sha
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(unix)]
fn commit_stages_and_commits_repos_in_parallel() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    let backend_feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    let frontend_feature = workspace.join(".knit/worktrees/venue-capacity/frontend");

    append_line(&backend_feature.join("app.txt"), "parallel backend commit");
    append_line(&frontend_feature.join("app.txt"), "parallel frontend commit");

    let gate = root.join("commit-gate");
    install_parallel_gate_hook(&backend_feature, "pre-commit", &gate, "backend", "frontend");
    install_parallel_gate_hook(&frontend_feature, "pre-commit", &gate, "frontend", "backend");

    let commit = knit(
        &workspace,
        ["commit", "--stage", "-m", "Parallel commit"],
    );
    assert!(commit.contains("backend"));
    assert!(commit.contains("frontend"));
    assert!(commit.contains("Recorded commit group"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_pushes_creates_records_and_syncs_cross_links() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
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
    assert!(status.contains("not landed"));
    assert!(status.contains("Next:"));
    assert!(status.contains("knit land"));

    let knit_status = knit(&workspace, ["status"]);
    assert!(knit_status.contains("Publications:"));
    assert!(knit_status.contains("not landed"));
    assert!(knit_status.contains("knit land"));

    let land_plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(land_plan.contains("Lands into:"));
    assert!(land_plan.contains("review object's base branch"));
    assert!(land_plan.contains("knit land apply"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_pr_create_uses_github_api_without_checkout_prompt() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact publish"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-publish/backend");
    append_line(&backend_feature.join("app.txt"), "artifact PR change");
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Artifact PR change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);

    let artifact = workspace.join(".knit/bundles/artifact-publish.bundle.json");
    let mut artifact_payload: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    artifact_payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&artifact_payload).unwrap(),
    )
    .unwrap();

    let out = root.join("artifact-publish.out.bundle.json");
    let create = knit_with_fake_gh(
        &root,
        vec![
            "publish".to_string(),
            "github".to_string(),
            "create".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
            "--no-push".to_string(),
            "--no-sync".to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert!(create.contains("created"));
    assert!(!fake_gh_dir.join("create-backend.args").exists());

    let find_endpoint = fs::read_to_string(fake_gh_dir.join("api-backend-find.endpoint")).unwrap();
    assert_eq!(
        find_endpoint.trim(),
        "repos/acme/backend/pulls?state=all&head=acme%3Aknit%2Fartifact-publish&base=main&per_page=1"
    );
    let endpoint = fs::read_to_string(fake_gh_dir.join("api-backend.endpoint")).unwrap();
    assert_eq!(endpoint.trim(), "repos/acme/backend/pulls");
    let prompt = fs::read_to_string(fake_gh_dir.join("api-backend.prompt")).unwrap();
    assert_eq!(prompt.trim(), "1");

    let payload: Value =
        serde_json::from_str(&fs::read_to_string(fake_gh_dir.join("api-backend.json")).unwrap())
            .unwrap();
    assert_eq!(payload["base"].as_str(), Some("main"));
    assert_eq!(payload["head"].as_str(), Some("knit/artifact-publish"));
    assert_eq!(
        payload["title"].as_str(),
        Some("artifact publish (backend)")
    );
    assert!(payload["body"]
        .as_str()
        .unwrap()
        .contains("This PR is part of Knit bundle `artifact-publish`."));

    let published: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        published["publications"][0]["url"].as_str(),
        Some("https://github.com/acme/backend/pull/101")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_pr_create_can_use_curl_ipv4_transport() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact publish"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-publish/backend");
    append_line(&backend_feature.join("app.txt"), "artifact PR change");
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Artifact PR change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    write_fake_curl(&fake_bin, &fake_gh_dir);

    let artifact = workspace.join(".knit/bundles/artifact-publish.bundle.json");
    let mut artifact_payload: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    artifact_payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&artifact_payload).unwrap(),
    )
    .unwrap();

    let out = root.join("artifact-publish.out.bundle.json");
    let create = knit_with_fake_gh_env(
        &root,
        vec![
            "publish".to_string(),
            "github".to_string(),
            "create".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
            "--no-push".to_string(),
            "--no-sync".to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &[
            ("GH_TOKEN", "gho_fake_token"),
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
        ],
    );
    assert!(create.contains("backend"));
    assert!(create.contains("created"));
    assert!(!fake_gh_dir.join("api-backend.endpoint").exists());
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("curl.ipv4"))
            .unwrap()
            .trim(),
        "1"
    );
    assert!(fs::read_to_string(fake_gh_dir.join("curl.netrc"))
        .unwrap()
        .contains("password gho_fake_token"));

    let payload: Value = serde_json::from_str(
        &fs::read_to_string(fake_gh_dir.join("curl-backend-create.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(payload["base"].as_str(), Some("main"));
    assert_eq!(payload["head"].as_str(), Some("knit/artifact-publish"));

    let published: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        published["publications"][0]["url"].as_str(),
        Some("https://github.com/acme/backend/pull/101")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_pr_create_reuses_existing_pr_found_with_github_api() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact publish"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-publish/backend");
    append_line(&backend_feature.join("app.txt"), "artifact PR change");
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Artifact PR change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    fs::write(fake_gh_dir.join("existing-backend"), "").unwrap();

    let artifact = workspace.join(".knit/bundles/artifact-publish.bundle.json");
    let mut artifact_payload: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    artifact_payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&artifact_payload).unwrap(),
    )
    .unwrap();

    let out = root.join("artifact-publish.out.bundle.json");
    let create = knit_with_fake_gh(
        &root,
        vec![
            "publish".to_string(),
            "github".to_string(),
            "create".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
            "--no-push".to_string(),
            "--no-sync".to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert!(create.contains("exists"));
    assert!(!fake_gh_dir.join("api-backend.json").exists());
    assert!(!fake_gh_dir.join("create-backend.args").exists());

    let find_endpoint = fs::read_to_string(fake_gh_dir.join("api-backend-find.endpoint")).unwrap();
    assert_eq!(
        find_endpoint.trim(),
        "repos/acme/backend/pulls?state=all&head=acme%3Aknit%2Fartifact-publish&base=main&per_page=1"
    );

    let published: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        published["publications"][0]["url"].as_str(),
        Some("https://github.com/acme/backend/pull/101")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn artifact_land_apply_can_use_curl_ipv4_transport() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "artifact publish"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/artifact-publish/backend");
    append_line(&backend_feature.join("app.txt"), "artifact land change");
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Artifact land change"],
    );

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    write_fake_curl(&fake_bin, &fake_gh_dir);

    let artifact = workspace.join(".knit/bundles/artifact-publish.bundle.json");
    let mut artifact_payload: Value =
        serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    artifact_payload["repos"][0]["remote"] = json!("https://github.com/acme/backend.git");
    artifact_payload["publications"] = json!([
        {
            "repoId": "backend",
            "provider": "github",
            "kind": "pull_request",
            "number": 101,
            "url": "https://github.com/acme/backend/pull/101",
            "baseBranch": "main",
            "headBranch": "knit/artifact-publish",
            "state": "OPEN",
            "title": "artifact publish (backend)",
            "updatedAt": "2026-06-06T00:00:00.000Z"
        }
    ]);
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&artifact_payload).unwrap(),
    )
    .unwrap();

    let out = root.join("artifact-land.out.bundle.json");
    let landed = knit_with_fake_gh_env(
        &root,
        vec![
            "land".to_string(),
            "apply".to_string(),
            "--from-artifact".to_string(),
            artifact.to_string_lossy().to_string(),
            "--out".to_string(),
            out.to_string_lossy().to_string(),
        ],
        &fake_bin,
        &fake_gh_dir,
        &[
            ("GH_TOKEN", "gho_fake_token"),
            ("KNIT_GITHUB_API_TRANSPORT", "curl-ipv4"),
        ],
    );
    assert!(landed.contains("checks backend"), "{landed}");
    assert!(landed.contains("merged backend"), "{landed}");
    assert!(!fake_gh_dir.join("merge-order.txt").exists());
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("curl.ipv4"))
            .unwrap()
            .trim(),
        "1"
    );

    let payload: Value = serde_json::from_str(
        &fs::read_to_string(fake_gh_dir.join("curl-backend-merge.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(payload["merge_method"].as_str(), Some("merge"));
    assert_eq!(payload["sha"].as_str(), Some("backend-head"));

    let landed_bundle: Value = serde_json::from_str(&fs::read_to_string(out).unwrap()).unwrap();
    assert_eq!(
        landed_bundle["publications"][0]["state"].as_str(),
        Some("MERGED")
    );
    assert!(landed_bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["type"].as_str() == Some("feature.landed")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_create_can_override_base_branch() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "release target"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let backend_feature = workspace.join(".knit/worktrees/release-target/backend");
    append_line(&backend_feature.join("app.txt"), "release PR change");
    knit(&workspace, ["commit", "--stage", "-m", "Release PR change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);

    let create = knit_with_fake_gh(
        &workspace,
        [
            "publish",
            "github",
            "create",
            "--no-sync",
            "--base",
            "release",
        ],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(create.contains("backend"));
    assert_eq!(
        fs::read_to_string(fake_gh_dir.join("create-backend.base"))
            .unwrap()
            .trim(),
        "release"
    );

    let bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/release-target.bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        bundle["publications"][0]["baseBranch"].as_str(),
        Some("release")
    );

    let rerun = knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(rerun.contains("exists"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_plan_and_apply_merges_recorded_publications_with_fake_gh() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
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

    let plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(plan.contains("Land plan"));
    assert!(plan.contains("merge-backend"));
    assert!(plan.contains("merge-frontend"));
    assert!(plan.contains("knit land apply"));
    let plan_path = workspace.join(".knit/land-plans/venue-capacity.land.json");
    assert!(plan_path.exists());
    let generated_plan: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    let steps = generated_plan["steps"].as_array().unwrap();
    assert_eq!(steps[0]["method"].as_str(), Some("merge"));
    assert_eq!(steps[1]["method"].as_str(), Some("merge"));
    assert!(!fake_gh_dir.join("merge-order.txt").exists());

    let existing_plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(existing_plan.contains("Land plan"));
    assert!(!fake_gh_dir.join("merge-order.txt").exists());

    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("Feature landed"));
    // This plan sets no repoOrder, so merges share a wave and run in parallel;
    // their relative order is unspecified, so compare as a set.
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    let mut order_lines = order.lines().collect::<Vec<_>>();
    order_lines.sort_unstable();
    assert_eq!(order_lines, vec!["backend", "frontend"]);
    let methods = fs::read_to_string(fake_gh_dir.join("merge-methods.txt")).unwrap();
    let mut method_lines = methods.lines().collect::<Vec<_>>();
    method_lines.sort_unstable();
    assert_eq!(method_lines, vec!["backend --merge", "frontend --merge"]);

    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("feature.landed"));
    assert_eq!(latest["provider"].as_str(), Some("github"));
    assert_eq!(latest["repoIds"].as_array().unwrap().len(), 2);
    assert_eq!(latest["publicationUrls"].as_array().unwrap().len(), 2);
    let landed_node_id = latest["id"].as_str().unwrap().to_string();
    assert!(workspace.join(".knit/land-runs").exists());
    assert!(knit(&workspace, ["bundle", "validate"]).contains("Bundle valid"));
    assert!(knit(&workspace, ["log", "-1"]).contains("landed"));
    let sync_error = knit_fails(&workspace, ["land", "sync"]);
    assert!(sync_error.contains("No KnitHub remote configured"));

    let revert_plan = knit_with_fake_gh(&workspace, ["revert", "HEAD"], &fake_bin, &fake_gh_dir);
    assert!(revert_plan.contains("Revert plan"), "{revert_plan}");
    assert!(revert_plan.contains("Provider: github"), "{revert_plan}");
    assert!(revert_plan.contains("prRevert"), "{revert_plan}");
    assert!(
        revert_plan.contains("https://github.com/acme/backend/pull/101"),
        "{revert_plan}"
    );
    assert!(
        revert_plan.contains("https://github.com/acme/frontend/pull/202"),
        "{revert_plan}"
    );

    let revert_apply = knit_with_fake_gh(
        &workspace,
        ["revert", "HEAD", "--apply"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        revert_apply.contains("Recorded PR revert group"),
        "{revert_apply}"
    );
    let revert_order = fs::read_to_string(fake_gh_dir.join("revert-order.txt")).unwrap();
    let mut revert_order_lines = revert_order.lines().collect::<Vec<_>>();
    revert_order_lines.sort_unstable();
    assert_eq!(revert_order_lines, vec!["backend", "frontend"]);
    let backend_revert_body = fs::read_to_string(fake_gh_dir.join("revert-backend.md")).unwrap();
    assert!(
        backend_revert_body.contains(&format!("Knit-Reverts: {landed_node_id}")),
        "{backend_revert_body}"
    );

    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("pr.revert"));
    assert_eq!(
        latest["targetNodeId"].as_str(),
        Some(landed_node_id.as_str())
    );
    assert_eq!(latest["provider"].as_str(), Some("github"));
    assert_eq!(latest["publicationUrls"].as_array().unwrap().len(), 2);
    assert!(bundle["publications"]
        .as_array()
        .unwrap()
        .iter()
        .any(|publication| {
            publication["repoId"].as_str() == Some("backend")
                && publication["number"].as_u64() == Some(901)
                && publication["state"].as_str() == Some("OPEN")
        }));
    assert!(bundle["publications"]
        .as_array()
        .unwrap()
        .iter()
        .any(|publication| {
            publication["repoId"].as_str() == Some("frontend")
                && publication["number"].as_u64() == Some(902)
                && publication["state"].as_str() == Some("OPEN")
        }));
    assert!(knit(&workspace, ["bundle", "validate"]).contains("Bundle valid"));
    assert!(knit(&workspace, ["log", "-1"]).contains("pr revert"));
    let show_revert = knit(&workspace, ["show", "HEAD"]);
    assert!(show_revert.contains("pr.revert"), "{show_revert}");
    assert!(show_revert.contains(&landed_node_id), "{show_revert}");
    assert!(
        show_revert.contains("https://github.com/acme/backend/pull/901"),
        "{show_revert}"
    );

    let mut stale_bundle = read_bundle(&workspace);
    stale_bundle["publications"] = json!([]);
    fs::write(
        workspace.join(".knit/bundles/venue-capacity.bundle.json"),
        format!("{}\n", serde_json::to_string_pretty(&stale_bundle).unwrap()),
    )
    .unwrap();
    let stale_status = knit_with_fake_gh(&workspace, ["land", "status"], &fake_bin, &fake_gh_dir);
    assert!(!stale_status.contains("publication missing"));
    assert!(stale_status.contains("https://github.com/acme/backend/pull/101"));
    assert!(stale_status.contains("https://github.com/acme/frontend/pull/202"));

    fs::remove_dir_all(root).unwrap();
}

/// Create a two-repo bundle and publish its PRs through the fake `gh`, returning
/// the workspace plus the fake-gh paths so tests can toggle PR state via markers.
fn publish_two_repo_bundle(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
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
    (workspace, fake_bin, fake_gh_dir)
}

#[test]
fn land_apply_skips_already_merged_pr() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);

    // backend is already merged with no prior run recorded; a fresh land apply
    // must treat it as a satisfied step, not bail with "expected OPEN".
    fs::write(fake_gh_dir.join("merged-backend"), "").unwrap();
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("Feature landed"), "{apply}");
    // Only frontend should be merged; backend was skipped as already merged.
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(
        order.lines().collect::<Vec<_>>(),
        vec!["frontend"],
        "{order}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_check_reports_pr_readiness() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);

    // Both PRs open, clean, no required checks -> ready.
    let check = knit_with_fake_gh(&workspace, ["land", "check"], &fake_bin, &fake_gh_dir);
    assert!(check.contains("Readiness:"), "{check}");
    assert!(check.contains("backend"), "{check}");
    assert!(check.contains("frontend"), "{check}");
    assert!(check.contains("ready"), "{check}");

    // backend merged, frontend conflicting -> distinct verdicts + update hint.
    fs::write(fake_gh_dir.join("merged-backend"), "").unwrap();
    fs::write(fake_gh_dir.join("conflict-frontend"), "").unwrap();
    let check2 = knit_with_fake_gh(&workspace, ["land", "check"], &fake_bin, &fake_gh_dir);
    assert!(check2.contains("already landed"), "{check2}");
    assert!(check2.contains("conflict"), "{check2}");
    assert!(check2.contains("knit land update"), "{check2}");

    // `publish status --live` surfaces the same readiness columns.
    let live = knit_with_fake_gh(
        &workspace,
        ["publish", "status", "--live"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(live.contains("verdict"), "{live}");
    assert!(live.contains("conflict"), "{live}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_conflict_points_to_land_update() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);

    fs::write(fake_gh_dir.join("conflict-backend"), "").unwrap();
    let error = knit_fails_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(error.contains("merge conflicts"), "{error}");
    assert!(error.contains("knit land update"), "{error}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_landing_template_orders_merges_and_runs_deploy_from_base_checkout() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, backend_collaborator) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["project", "init", "arbient"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    fs::write(backend_collaborator.join("base.txt"), "ready for deploy\n").unwrap();
    git(&backend_collaborator, ["add", "base.txt"]);
    git(
        &backend_collaborator,
        ["commit", "-m", "Deploy base update"],
    );
    git(&backend_collaborator, ["push", "origin", "main"]);

    let deploy_pwd = root.join("deploy-pwd.txt");
    let deploy_branch = root.join("deploy-branch.txt");
    let deploy_script = format!(
        "pwd > '{}' && git rev-parse --abbrev-ref HEAD > '{}' && test -f base.txt",
        deploy_pwd.display(),
        deploy_branch.display()
    );
    let project_path = workspace.join(".knit/projects/arbient.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "merge": {
            "repoOrder": ["frontend", "backend"],
            "method": "merge",
            "waitForChecks": true,
            "requiredChecksOnly": true,
            "deleteBranch": false
        },
        "deployments": [
            {
                "id": "deploy-backend",
                "repoId": "backend",
                "checkout": { "branch": "main", "remote": "origin", "update": "pull" },
                "command": ["sh", "-c", deploy_script]
            },
            {
                "id": "deploy-frontend",
                "repoId": "frontend",
                "mode": "push"
            }
        ]
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "start", "venue capacity"]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend project landing",
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"),
        "frontend project landing",
    );
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Project landing change"],
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
    let plan: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    assert_eq!(plan["sourceProjectId"].as_str(), Some("arbient"));
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(steps[0]["id"].as_str(), Some("merge-frontend"));
    assert_eq!(steps[1]["id"].as_str(), Some("merge-backend"));
    assert_eq!(steps[2]["type"].as_str(), Some("deploy"));
    assert_eq!(steps[2]["id"].as_str(), Some("deploy-backend"));
    assert_eq!(
        steps[2]["needs"].as_array().unwrap()[0].as_str(),
        Some("merge-backend")
    );
    assert_eq!(steps[3]["id"].as_str(), Some("deploy-frontend"));
    assert_eq!(
        steps[3]["needs"].as_array().unwrap()[0].as_str(),
        Some("merge-frontend")
    );

    let apply = knit_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    assert!(apply.contains("deploy-backend"));
    assert!(apply.contains("deploy-frontend"));
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(
        order.lines().collect::<Vec<_>>(),
        vec!["frontend", "backend"]
    );
    assert!(fs::read_to_string(&deploy_pwd)
        .unwrap()
        .contains(".knit/land-worktrees/venue-capacity/backend/main"));
    assert_eq!(fs::read_to_string(&deploy_branch).unwrap().trim(), "HEAD");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_update_merges_base_and_records_explicit_node() {
    let root = unique_temp_dir();
    let (backend_remote, backend, backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    let backend_feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    append_line(&backend_feature.join("app.txt"), "backend feature update");
    knit(&workspace, ["commit", "--stage", "-m", "Feature update"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    fs::write(
        backend_collaborator.join("base.txt"),
        "base branch update\n",
    )
    .unwrap();
    git(&backend_collaborator, ["add", "base.txt"]);
    git(
        &backend_collaborator,
        ["commit", "-m", "Base branch update"],
    );
    git(&backend_collaborator, ["push", "origin", "main"]);

    let update = knit_with_fake_gh(
        &workspace,
        ["land", "update", "--push"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(update.contains("backend"));
    assert!(update.contains("updated"));
    assert!(update.contains("pushed"));

    let local_head = git(&backend_feature, ["rev-parse", "HEAD"]);
    assert_eq!(
        git(
            &backend_remote,
            ["rev-parse", "refs/heads/knit/venue-capacity"],
        ),
        local_head
    );

    let bundle = read_bundle(&workspace);
    let latest = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"].as_str(), Some("land.update"));
    assert_eq!(latest["provider"].as_str(), Some("github"));
    let repo_changes = latest["repoChanges"].as_array().unwrap();
    assert_eq!(repo_changes.len(), 1);
    assert_eq!(repo_changes[0]["repoId"].as_str(), Some("backend"));
    assert_eq!(
        repo_changes[0]["afterSha"].as_str(),
        Some(local_head.trim())
    );
    assert_eq!(
        bundle["repos"][0]["headSha"].as_str(),
        Some(local_head.trim())
    );

    let log = knit(&workspace, ["log", "-1"]);
    assert!(log.contains("updated from base"));
    let show = knit(&workspace, ["show", "HEAD"]);
    assert!(show.contains("land.update"));
    assert!(show.contains("Base branch update"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_resume_skips_succeeded_steps_and_retries_failed_run_steps() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
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

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
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

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
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
    assert!(failed.contains("required checks failed: test"));
    assert!(!fake_gh_dir.join("merge-order.txt").exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_apply_treats_no_required_checks_as_ready() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "docs cleanup"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    append_line(
        &workspace.join(".knit/worktrees/docs-cleanup/backend/app.txt"),
        "docs cleanup landing",
    );
    knit(&workspace, ["commit", "--stage", "-m", "Docs cleanup"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "github", "create", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    let plan = knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    assert!(plan.contains("Land plan"));
    let plan_json: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/land-plans/docs-cleanup.land.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(plan_json["steps"][0]["waitForChecks"].as_bool(), Some(true));

    let status = knit_with_fake_gh_env(
        &workspace,
        ["land", "status"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_NO_REQUIRED_CHECKS_ERROR", "1")],
    );
    assert!(status.contains("checks passed (no required checks)"));
    assert!(!status.contains("checks unavailable"));

    let apply = knit_with_fake_gh_env(
        &workspace,
        ["land", "apply"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_NO_REQUIRED_CHECKS_ERROR", "1")],
    );
    assert!(apply.contains("Feature landed"));
    let run_status = knit_with_fake_gh_env(
        &workspace,
        ["land", "status"],
        &fake_bin,
        &fake_gh_dir,
        &[("GH_FAKE_NO_REQUIRED_CHECKS_ERROR", "1")],
    );
    assert!(run_status.contains("checks passed (no required checks)"));
    let order = fs::read_to_string(fake_gh_dir.join("merge-order.txt")).unwrap();
    assert_eq!(order.trim(), "backend");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn checkpoint_records_non_git_ledger_note() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
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

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/venue-capacity/backend");
    let head_before_close = git(&feature, ["rev-parse", "HEAD"]);

    let close = knit(&workspace, ["bundle", "close", "--reason", "merged"]);
    assert!(close.contains("Closed bundle"));
    assert!(close.contains("Preserved"));
    assert!(close.contains("worktrees and local feature branches"));
    assert!(close.contains("knit bundle delete venue-capacity --force --worktrees --branches"));

    let status = knit(&workspace, ["status"]);
    assert!(status.contains("State: closed"));
    assert!(status.contains("knit/venue-capacity"));
    assert!(status.contains(".knit/worktrees/venue-capacity/backend"));
    assert!(status.contains("ledger marker only"));

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

    let second_close = knit_fails(&workspace, ["bundle", "close"]);
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

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
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

    knit(&workspace, ["bundle", "worktree"]);
    assert!(worktree.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_delete_can_remove_generated_worktrees_and_force_delete_branches() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "start", "throwaway"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let worktree = workspace.join(".knit/worktrees/throwaway/backend");
    append_line(&worktree.join("app.txt"), "throwaway change");
    knit(&workspace, ["commit", "--stage", "-m", "Throwaway change"]);
    assert!(git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));

    let safe_delete = knit_fails(
        &workspace,
        [
            "bundle",
            "delete",
            "throwaway",
            "--force",
            "--worktrees",
            "--branches",
        ],
    );
    assert!(safe_delete.contains("failed to delete feature branches"));
    assert!(workspace
        .join(".knit/bundles/throwaway.bundle.json")
        .exists());
    assert!(!worktree.exists());
    assert!(git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));

    let forced_delete = knit(
        &workspace,
        [
            "bundle",
            "delete",
            "throwaway",
            "--force",
            "--worktrees",
            "--branches",
            "--force-branches",
        ],
    );
    assert!(forced_delete.contains("Deleted bundle"));
    assert!(!workspace
        .join(".knit/bundles/throwaway.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/throwaway.bundle.json")
        .exists());
    assert!(!git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));
    let deleted: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/deleted/bundles/throwaway.bundle.json")).unwrap(),
    )
    .unwrap();
    assert!(deleted["repos"][0]["worktreePath"].is_null());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn delete_recovers_generated_worktree_when_recorded_path_was_lost() {
    // A bundle synced back from a remote is localized, which clears the
    // local-only worktreePath even though the generated checkout still exists
    // and holds its feature branch. Cleanup must fall back to the conventional
    // location so it removes the worktree and frees the branch for deletion.
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "start", "throwaway"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let worktree = workspace.join(".knit/worktrees/throwaway/backend");
    append_line(&worktree.join("app.txt"), "throwaway change");
    knit(&workspace, ["commit", "--stage", "-m", "Throwaway change"]);

    // Simulate the post-localize state: the recorded worktree path is gone.
    let bundle_path = workspace.join(".knit/bundles/throwaway.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    bundle["repos"][0]["worktreePath"] = Value::Null;
    fs::write(&bundle_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();
    assert!(worktree.exists());
    assert!(git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));

    let deleted = knit(
        &workspace,
        [
            "bundle",
            "delete",
            "throwaway",
            "--force",
            "--worktrees",
            "--branches",
            "--force-branches",
        ],
    );
    assert!(deleted.contains("Deleted bundle"));
    assert!(!worktree.exists());
    assert!(!git(&backend, ["branch", "--list", "knit/throwaway"]).contains("knit/throwaway"));
    assert!(!bundle_path.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_prune_removes_only_bundles_with_all_recorded_prs_merged() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["bundle", "merged cleanup"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    write_bundle_publications(&workspace, "merged-cleanup", "MERGED");

    knit(&workspace, ["bundle", "start", "open cleanup"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    write_bundle_publications(&workspace, "open-cleanup", "OPEN");

    let preview = knit(&workspace, ["bundle", "prune", "--no-refresh"]);
    assert!(preview.contains("Dead bundle candidates"));
    assert!(preview.contains("merged-cleanup"));
    assert!(!preview.contains("open-cleanup"));
    assert!(workspace
        .join(".knit/bundles/merged-cleanup.bundle.json")
        .exists());

    let pruned = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--apply",
            "--worktrees",
            "--branches",
        ],
    );
    assert!(pruned.contains("Deleted bundle"));
    assert!(pruned.contains("Pruned"));
    assert!(!workspace
        .join(".knit/bundles/merged-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/merged-cleanup.bundle.json")
        .exists());
    assert!(!workspace
        .join(".knit/worktrees/merged-cleanup/backend")
        .exists());
    assert!(
        !git(&backend, ["branch", "--list", "knit/merged-cleanup"]).contains("knit/merged-cleanup")
    );

    assert!(workspace
        .join(".knit/bundles/open-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/worktrees/open-cleanup/backend")
        .exists());
    assert!(git(&backend, ["branch", "--list", "knit/open-cleanup"]).contains("knit/open-cleanup"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_prune_refreshes_stale_publication_states_by_default() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    write_bundle_publications(&workspace, "venue-capacity", "OPEN");

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    fs::write(fake_gh_dir.join("merged-backend"), "").unwrap();
    fs::write(fake_gh_dir.join("merged-frontend"), "").unwrap();

    let preview = knit_with_fake_gh(&workspace, ["bundle", "prune"], &fake_bin, &fake_gh_dir);
    assert!(preview.contains("Dead bundle candidates"));
    assert!(preview.contains("venue-capacity"));

    let bundle = read_bundle(&workspace);
    assert!(bundle["publications"]
        .as_array()
        .unwrap()
        .iter()
        .all(|publication| publication["state"].as_str() == Some("MERGED")));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_prune_removes_clean_dead_work_with_missing_publications() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["bundle", "partial landed"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    write_bundle_publications_for_repos(&workspace, "partial-landed", "MERGED", &["backend"]);

    knit(&workspace, ["bundle", "start", "abandoned cleanup"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);

    knit(&workspace, ["bundle", "start", "dirty cleanup"]);
    knit(&workspace, ["bundle", "add", frontend.to_str().unwrap()]);
    let dirty_feature = workspace.join(".knit/worktrees/dirty-cleanup/frontend");
    append_line(&dirty_feature.join("app.txt"), "dirty local edit");

    let preview = knit(&workspace, ["bundle", "prune", "--no-refresh", "--worktrees"]);
    assert!(preview.contains("partial-landed"));
    assert!(preview.contains("recorded PRs are merged"));
    assert!(preview.contains("abandoned-cleanup"));
    assert!(preview.contains("no recorded PRs and no pending changes"));
    assert!(!preview.contains("dirty-cleanup"));

    let pruned = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--apply",
            "--worktrees",
            "--branches",
            "--force-branches",
        ],
    );
    assert!(pruned.contains("partial-landed"));
    assert!(pruned.contains("abandoned-cleanup"));
    assert!(!workspace
        .join(".knit/bundles/partial-landed.bundle.json")
        .exists());
    assert!(!workspace
        .join(".knit/bundles/abandoned-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/partial-landed.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/abandoned-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/bundles/dirty-cleanup.bundle.json")
        .exists());
    assert!(dirty_feature.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_prune_untracked_flag_prunes_untracked_only_dead_work() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    // Dead work whose only uncommitted content is an untracked file.
    knit(&workspace, ["bundle", "start", "stray cleanup"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let stray_feature = workspace.join(".knit/worktrees/stray-cleanup/backend");
    fs::write(stray_feature.join("PROOF.md"), "untracked proof\n").unwrap();

    // Dead work with a tracked modification must stay protected even with --untracked.
    knit(&workspace, ["bundle", "start", "dirty cleanup"]);
    knit(&workspace, ["bundle", "add", frontend.to_str().unwrap()]);
    let dirty_feature = workspace.join(".knit/worktrees/dirty-cleanup/frontend");
    append_line(&dirty_feature.join("app.txt"), "dirty local edit");

    // Default prune surfaces the untracked-only bundle separately and does not
    // treat it as a deletable candidate.
    let preview = knit(&workspace, ["bundle", "prune", "--no-refresh"]);
    assert!(preview.contains("Blocked by untracked files"));
    assert!(preview.contains("stray-cleanup"));
    assert!(!preview.contains("Dead bundle candidates"));

    // --report names every bundle and its status, including kept ones.
    let report = knit(&workspace, ["bundle", "prune", "--no-refresh", "--report"]);
    assert!(report.contains("Bundle report:"));
    assert!(report.contains("prunable with --untracked"));
    assert!(report.contains("dirty-cleanup"));
    assert!(report.contains("uncommitted tracked changes"));

    // --untracked promotes the untracked-only bundle to a real candidate while
    // still protecting the tracked-change bundle.
    let untracked_preview = knit(&workspace, ["bundle", "prune", "--no-refresh", "--untracked"]);
    assert!(untracked_preview.contains("Dead bundle candidates"));
    assert!(untracked_preview.contains("stray-cleanup"));
    assert!(untracked_preview.contains("discards untracked files"));
    assert!(!untracked_preview.contains("dirty-cleanup"));

    let pruned = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--untracked",
            "--apply",
            "--worktrees",
            "--branches",
            "--force-branches",
        ],
    );
    assert!(pruned.contains("stray-cleanup"));
    assert!(!workspace
        .join(".knit/bundles/stray-cleanup.bundle.json")
        .exists());
    assert!(workspace
        .join(".knit/deleted/bundles/stray-cleanup.bundle.json")
        .exists());
    assert!(!stray_feature.exists());

    // The tracked-change bundle and its worktree survive.
    assert!(workspace
        .join(".knit/bundles/dirty-cleanup.bundle.json")
        .exists());
    assert!(dirty_feature.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn prune_removes_orphan_worktree_dirs_without_bundle_artifacts() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");

    knit(&workspace, ["bundle", "dirty active"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let active_feature = workspace.join(".knit/worktrees/dirty-active/backend");
    append_line(&active_feature.join("app.txt"), "keep me");

    let empty_orphan = workspace.join(".knit/worktrees/empty-orphan/nested/leaf");
    fs::create_dir_all(&empty_orphan).unwrap();
    let dirty_orphan = workspace.join(".knit/worktrees/dirty-orphan");
    fs::create_dir_all(&dirty_orphan).unwrap();
    fs::write(dirty_orphan.join("note.txt"), "untracked work\n").unwrap();

    let preview = knit(&workspace, ["bundle", "prune", "--no-refresh", "--worktrees"]);
    assert!(preview.contains("Orphan worktree candidates"));
    assert!(preview.contains("empty-orphan"));
    assert!(preview.contains("Blocked orphan worktrees"));
    assert!(preview.contains("dirty-orphan"));
    assert!(preview.contains("--force"));
    assert!(!preview.contains("dirty-active"));

    let pruned = knit(
        &workspace,
        ["bundle", "prune", "--no-refresh", "--apply", "--worktrees"],
    );
    assert!(pruned.contains("removed orphan worktree"));
    assert!(!workspace.join(".knit/worktrees/empty-orphan").exists());
    assert!(dirty_orphan.exists());
    assert!(active_feature.exists());

    let forced = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--apply",
            "--worktrees",
            "--force",
        ],
    );
    assert!(forced.contains("removed orphan worktree"));
    assert!(!dirty_orphan.exists());
    assert!(active_feature.exists());
    assert!(workspace
        .join(".knit/bundles/dirty-active.bundle.json")
        .exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn prune_can_remove_generated_worktrees_local_branches_and_remote_branches() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "remote cleanup"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/remote-cleanup/backend");
    append_line(&feature.join("app.txt"), "remote cleanup change");
    knit(
        &workspace,
        ["commit", "--stage", "-m", "Remote cleanup change"],
    );
    git(&feature, ["push", "origin", "knit/remote-cleanup"]);
    assert!(git_success(
        &backend_remote,
        ["rev-parse", "--verify", "refs/heads/knit/remote-cleanup"],
    ));
    assert!(git_success(
        &backend,
        [
            "rev-parse",
            "--verify",
            "refs/remotes/origin/knit/remote-cleanup",
        ],
    ));

    write_bundle_publications(&workspace, "remote-cleanup", "MERGED");
    let preview = knit(&workspace, ["bundle", "prune", "--no-refresh", "--all"]);
    assert!(preview.contains("knit bundle prune --apply --all"));

    let pruned = knit(
        &workspace,
        [
            "bundle",
            "prune",
            "--no-refresh",
            "--apply",
            "--worktrees",
            "--branches",
            "--force-branches",
            "--remote-branches",
        ],
    );
    assert!(pruned.contains("Deleted bundle"));
    assert!(pruned.contains("origin/knit/remote-cleanup"));
    assert!(!feature.exists());
    assert!(!git_success(
        &backend,
        ["rev-parse", "--verify", "refs/heads/knit/remote-cleanup"],
    ));
    assert!(!git_success(
        &backend_remote,
        ["rev-parse", "--verify", "refs/heads/knit/remote-cleanup"],
    ));
    assert!(!git_success(
        &backend,
        [
            "rev-parse",
            "--verify",
            "refs/remotes/origin/knit/remote-cleanup",
        ],
    ));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pull_feature_checkout_records_observed_git_movement() {
    let root = unique_temp_dir();
    let (_remote, backend, collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
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

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        ["bundle", "add", "--in-place", backend.to_str().unwrap()],
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
    let stage_failure = knit_fails(&workspace, ["add"]);
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

#[cfg(unix)]
fn install_parallel_push_hook(repo: &Path, gate: &Path, id: &str, peer: &str) {
    install_parallel_gate_hook(repo, "pre-push", gate, id, peer);
}

#[cfg(unix)]
fn install_parallel_gate_hook(repo: &Path, hook: &str, gate: &Path, id: &str, peer: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(gate).unwrap();
    let hook_path = git(repo, ["rev-parse", "--git-path", &format!("hooks/{hook}")]);
    let hook_path = PathBuf::from(hook_path.trim());
    let hook_path = if hook_path.is_absolute() {
        hook_path
    } else {
        repo.join(hook_path)
    };
    fs::create_dir_all(hook_path.parent().unwrap()).unwrap();
    fs::write(
        &hook_path,
        format!(
            r#"#!/bin/sh
set -eu
gate={gate}
id={id}
peer={peer}
touch "$gate/$id"
i=0
while [ ! -f "$gate/$peer" ]; do
  i=$((i + 1))
  if [ "$i" -ge 100 ]; then
    echo "timed out waiting for parallel push peer $peer" >&2
    exit 42
  fi
  sleep 0.05
done
"#,
            gate = shell_quote(&gate.to_string_lossy()),
            id = shell_quote(id),
            peer = shell_quote(peer)
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&hook_path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&hook_path, permissions).unwrap();
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn read_bundle(workspace: &Path) -> Value {
    let path = workspace.join(".knit/bundles/venue-capacity.bundle.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn write_bundle_publications(workspace: &Path, bundle_id: &str, state: &str) {
    write_bundle_publications_for_repos(workspace, bundle_id, state, &[]);
}

fn write_bundle_publications_for_repos(
    workspace: &Path,
    bundle_id: &str,
    state: &str,
    repo_ids: &[&str],
) {
    let path = workspace
        .join(".knit/bundles")
        .join(format!("{bundle_id}.bundle.json"));
    let mut bundle: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let publications = bundle["repos"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, repo)| repo_ids.is_empty() || repo_ids.contains(&repo["id"].as_str().unwrap()))
        .map(|(index, repo)| {
            let repo_id = repo["id"].as_str().unwrap();
            let head_branch = repo["featureBranch"].as_str().unwrap();
            let base_branch = repo["baseBranch"].as_str().unwrap();
            json!({
                "repoId": repo_id,
                "provider": "github",
                "kind": "pull_request",
                "number": (index + 1) as u64,
                "url": format!("https://github.com/acme/{repo_id}/pull/{}", index + 1),
                "baseBranch": base_branch,
                "headBranch": head_branch,
                "state": state,
                "updatedAt": "2026-05-22T00:00:00.000Z"
            })
        })
        .collect::<Vec<_>>();
    bundle["publications"] = json!(publications);
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&bundle).unwrap()),
    )
    .unwrap();
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

fn knit_with_env<I, S>(cwd: &Path, args: I, env: &[(&str, &str)]) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command.args(args).current_dir(cwd);
    for (key, value) in env {
        command.env(key, value);
    }
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

fn git_success<I, S>(cwd: &Path, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command.status().unwrap().success()
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

api_pr_json() {
  pr_repo="$1"
  number="$2"
  base="main"
  head="knit/artifact-publish"
  if [ -f "$GH_FAKE_DIR/api-$pr_repo.head" ]; then
    head="$(cat "$GH_FAKE_DIR/api-$pr_repo.head")"
  fi
  state="open"
  merged="false"
  if [ -f "$GH_FAKE_DIR/merged-$pr_repo" ]; then
    state="closed"
    merged="true"
  fi
  mergeable="true"
  mergestate="clean"
  if [ -f "$GH_FAKE_DIR/conflict-$pr_repo" ]; then
    mergeable="false"
    mergestate="dirty"
  fi
  printf '{"number":%s,"html_url":"https://github.com/acme/%s/pull/%s","state":"%s","title":"%s PR","body":"Existing body","draft":false,"head":{"ref":"%s","sha":"%s-head"},"base":{"ref":"%s"},"merged":%s,"mergeable":%s,"mergeable_state":"%s"}\n' "$number" "$pr_repo" "$number" "$state" "$pr_repo" "$head" "$pr_repo" "$base" "$merged" "$mergeable" "$mergestate"
}

if [ "$1" = "api" ]; then
  shift
  method="GET"
  endpoint=""
  input=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --method)
        method="$2"
        shift 2
        ;;
      --input)
        input="$2"
        shift 2
        ;;
      --jq)
        shift 2
        ;;
      -*)
        shift
        ;;
      *)
        if [ -z "$endpoint" ]; then
          endpoint="$1"
        fi
        shift
        ;;
      esac
  done
  endpoint_path="${endpoint%%\?*}"
  case "$endpoint_path" in
    repos/acme/backend/pulls|repos/acme/backend/pulls/*) pr_repo=backend ;;
    repos/acme/frontend/pulls|repos/acme/frontend/pulls/*) pr_repo=frontend ;;
    *) pr_repo=other ;;
  esac
  case "$pr_repo" in
    backend) number=101 ;;
    frontend) number=202 ;;
    *) number=303 ;;
  esac
  case "$endpoint_path" in
    repos/acme/*/pulls)
      if [ "$method" = "GET" ]; then
        printf '%s\n' "$method" > "$GH_FAKE_DIR/api-$pr_repo-find.method"
        printf '%s\n' "$endpoint" > "$GH_FAKE_DIR/api-$pr_repo-find.endpoint"
        printf '%s\n' "${GH_PROMPT_DISABLED:-}" > "$GH_FAKE_DIR/api-$pr_repo-find.prompt"
        if [ -f "$GH_FAKE_DIR/existing-$pr_repo" ]; then
          printf '['
          api_pr_json "$pr_repo" "$number"
          printf ']\n'
        else
          printf '[]\n'
        fi
      elif [ "$method" = "POST" ]; then
        if [ "$input" = "-" ]; then
          cat > "$GH_FAKE_DIR/api-$pr_repo.json"
          sed -n 's/.*"head":"\([^"]*\)".*/\1/p' "$GH_FAKE_DIR/api-$pr_repo.json" > "$GH_FAKE_DIR/api-$pr_repo.head"
        else
          : > "$GH_FAKE_DIR/api-$pr_repo.json"
        fi
        printf '%s\n' "$method" > "$GH_FAKE_DIR/api-$pr_repo.method"
        printf '%s\n' "$endpoint" > "$GH_FAKE_DIR/api-$pr_repo.endpoint"
        printf '%s\n' "${GH_PROMPT_DISABLED:-}" > "$GH_FAKE_DIR/api-$pr_repo.prompt"
        printf 'https://github.com/acme/%s/pull/%s\n' "$pr_repo" "$number"
      else
        echo "unexpected gh api method for pull collection: $method" >&2
        exit 1
      fi
      ;;
    repos/acme/*/pulls/*)
      number="${endpoint_path##*/}"
      if [ "$method" = "GET" ]; then
        printf '%s\n' "$method" > "$GH_FAKE_DIR/api-$pr_repo-view.method"
        printf '%s\n' "$endpoint" > "$GH_FAKE_DIR/api-$pr_repo-view.endpoint"
        printf '%s\n' "${GH_PROMPT_DISABLED:-}" > "$GH_FAKE_DIR/api-$pr_repo-view.prompt"
        api_pr_json "$pr_repo" "$number"
      elif [ "$method" = "PATCH" ]; then
        if [ "$input" = "-" ]; then
          cat > "$GH_FAKE_DIR/api-$pr_repo-edit.json"
        else
          : > "$GH_FAKE_DIR/api-$pr_repo-edit.json"
        fi
        printf '%s\n' "$method" > "$GH_FAKE_DIR/api-$pr_repo-edit.method"
        printf '%s\n' "$endpoint" > "$GH_FAKE_DIR/api-$pr_repo-edit.endpoint"
        printf '%s\n' "${GH_PROMPT_DISABLED:-}" > "$GH_FAKE_DIR/api-$pr_repo-edit.prompt"
        api_pr_json "$pr_repo" "$number"
      else
        echo "unexpected gh api method for pull item: $method" >&2
        exit 1
      fi
      ;;
    *)
      echo "unexpected gh api endpoint: $endpoint" >&2
      exit 1
      ;;
  esac
  exit 0
fi

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
    base="main"
    args="$*"
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --base)
          base="$2"
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    printf '%s\n' "$base" > "$GH_FAKE_DIR/create-$repo.base"
    printf '%s\n' "$args" > "$GH_FAKE_DIR/create-$repo.args"
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
    base="main"
    if [ -f "$GH_FAKE_DIR/create-$pr_repo.base" ]; then
      base="$(cat "$GH_FAKE_DIR/create-$pr_repo.base")"
    fi
    state="OPEN"
    title="$pr_repo PR"
    head="knit/venue-capacity"
    if [ -f "$GH_FAKE_DIR/revert-$pr_repo.number" ] && [ "$number" = "$(cat "$GH_FAKE_DIR/revert-$pr_repo.number")" ]; then
      state="OPEN"
      title="Revert $pr_repo PR"
      head="knit/revert-$pr_repo"
    elif [ -f "$GH_FAKE_DIR/merged-$pr_repo" ]; then
      state="MERGED"
    fi
    draft="false"
    if [ "${GH_FAKE_DRAFT:-0}" = "1" ]; then
      draft="true"
    fi
    mergeable="MERGEABLE"
    mergestate="CLEAN"
    if [ -f "$GH_FAKE_DIR/conflict-$pr_repo" ]; then
      mergeable="CONFLICTING"
      mergestate="DIRTY"
    fi
    review="${GH_FAKE_REVIEW:-}"
    printf '{"number":%s,"url":"%s","state":"%s","title":"%s","baseRefName":"%s","headRefName":"%s","body":"Existing body","isDraft":%s,"headRefOid":"%s-head","mergeable":"%s","mergeStateStatus":"%s","reviewDecision":"%s"}\n' "$number" "$url" "$state" "$title" "$base" "$head" "$draft" "$pr_repo" "$mergeable" "$mergestate" "$review"
    ;;
  edit)
    url="$1"
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    cat > "$GH_FAKE_DIR/edit-$pr_repo.md"
    printf '%s\n' "$url"
    ;;
  revert)
    url="$1"
    shift
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    title="Revert $pr_repo PR"
    body_written=0
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --title)
          title="$2"
          shift 2
          ;;
        --body-file)
          if [ "$2" = "-" ]; then
            cat > "$GH_FAKE_DIR/revert-$pr_repo.md"
          else
            cp "$2" "$GH_FAKE_DIR/revert-$pr_repo.md"
          fi
          body_written=1
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    if [ "$body_written" -eq 0 ]; then
      : > "$GH_FAKE_DIR/revert-$pr_repo.md"
    fi
    case "$pr_repo" in
      backend) number=901 ;;
      frontend) number=902 ;;
      *) number=903 ;;
    esac
    printf '%s\n' "$number" > "$GH_FAKE_DIR/revert-$pr_repo.number"
    printf '%s\n' "$title" > "$GH_FAKE_DIR/revert-$pr_repo.title"
    printf '%s\n' "$pr_repo" >> "$GH_FAKE_DIR/revert-order.txt"
    printf 'https://github.com/acme/%s/pull/%s\n' "$pr_repo" "$number"
    ;;
  checks)
    if [ "${GH_FAKE_NO_REQUIRED_CHECKS_ERROR:-0}" = "1" ]; then
      echo "no required checks reported" >&2
      exit 1
    fi
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
    method=""
    for arg in "$@"; do
      case "$arg" in
        --merge|--squash|--rebase) method="$arg" ;;
      esac
    done
    printf '%s %s\n' "$pr_repo" "$method" >> "$GH_FAKE_DIR/merge-methods.txt"
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

#[cfg(unix)]
fn write_fake_curl(fake_bin: &Path, fake_gh_dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(fake_bin).unwrap();
    fs::create_dir_all(fake_gh_dir).unwrap();
    let script = fake_bin.join("curl");
    fs::write(
        &script,
        r#"#!/bin/sh
set -eu

api_pr_json() {
  number="$1"
  state="open"
  merged="false"
  if [ -f "$GH_FAKE_DIR/merged-backend" ]; then
    state="closed"
    merged="true"
  fi
  printf '{"number":%s,"html_url":"https://github.com/acme/backend/pull/%s","state":"%s","title":"backend PR","body":"Existing body","draft":false,"head":{"ref":"knit/artifact-publish","sha":"backend-head"},"base":{"ref":"main"},"merged":%s,"mergeable":true,"mergeable_state":"clean"}\n' "$number" "$number" "$state" "$merged"
}

method="GET"
url=""
netrc=""
ipv4=0
data=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --request)
      method="$2"
      shift 2
      ;;
    --netrc-file)
      netrc="$2"
      shift 2
      ;;
    --ipv4)
      ipv4=1
      shift
      ;;
    --data-binary)
      data="$2"
      shift 2
      ;;
    --header|--connect-timeout|--max-time)
      shift 2
      ;;
    --silent|--show-error|--fail-with-body|--location)
      shift
      ;;
    http*)
      url="$1"
      shift
      ;;
    *)
      shift
      ;;
  esac
done

printf '%s\n' "$ipv4" > "$GH_FAKE_DIR/curl.ipv4"
if [ -n "$netrc" ]; then
  cat "$netrc" > "$GH_FAKE_DIR/curl.netrc"
fi

endpoint="${url#https://api.github.com/}"
endpoint_path="${endpoint%%\?*}"

case "$endpoint_path" in
  repos/acme/backend/pulls)
    if [ "$method" = "GET" ]; then
      printf '[]\n'
    elif [ "$method" = "POST" ]; then
      if [ "$data" = "@-" ]; then
        cat > "$GH_FAKE_DIR/curl-backend-create.json"
      fi
      api_pr_json 101
    else
      echo "unexpected curl method for pull collection: $method" >&2
      exit 1
    fi
    ;;
  repos/acme/backend/pulls/*/merge)
    if [ "$method" = "PUT" ]; then
      if [ "$data" = "@-" ]; then
        cat > "$GH_FAKE_DIR/curl-backend-merge.json"
      fi
      touch "$GH_FAKE_DIR/merged-backend"
      printf '{"merged":true,"message":"Pull Request successfully merged","sha":"merge-sha"}\n'
    else
      echo "unexpected curl method for merge: $method" >&2
      exit 1
    fi
    ;;
  repos/acme/backend/pulls/*)
    if [ "$method" = "GET" ]; then
      api_pr_json "${endpoint_path##*/}"
    elif [ "$method" = "PATCH" ]; then
      if [ "$data" = "@-" ]; then
        cat > "$GH_FAKE_DIR/curl-backend-edit.json"
      fi
      api_pr_json "${endpoint_path##*/}"
    else
      echo "unexpected curl method for pull item: $method" >&2
      exit 1
    fi
    ;;
  repos/acme/backend/commits/*/check-runs)
    if [ "$method" = "GET" ]; then
      printf '{"total_count":0,"check_runs":[]}\n'
    else
      echo "unexpected curl method for check runs: $method" >&2
      exit 1
    fi
    ;;
  repos/acme/backend/commits/*/status)
    if [ "$method" = "GET" ]; then
      printf '{"state":"success","statuses":[]}\n'
    else
      echo "unexpected curl method for statuses: $method" >&2
      exit 1
    fi
    ;;
  *)
    echo "unexpected curl endpoint: $endpoint" >&2
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
fn write_fake_curl(_fake_bin: &Path, _fake_gh_dir: &Path) {
    panic!("fake curl smoke test requires a unix-like shell");
}
