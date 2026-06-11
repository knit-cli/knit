mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;

#[test]
fn init_can_generate_agents_tutorial() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let output = knit(&workspace, ["bundle", "venue capacity", "--agents"]);
    assert!(output.contains("AGENTS.md"));

    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("This is a Knit workspace"));
    assert!(agents.contains("knit status"));
    assert!(agents.contains("knit init"));
    assert!(agents.contains("knit bundle \""));
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
    assert!(agents.contains("knit --bundle feature-a commit --all"));
    assert!(agents.contains("knit --bundle feature-a push --set-upstream"));
    assert!(agents.contains("Project JSON can define a default `landing` template"));
    assert!(agents.contains(".knit/land-plans/<bundle>.land.json"));
    assert!(agents.contains("gloss prepare"));

    fs::remove_file(workspace.join("AGENTS.md")).unwrap();
    let existing_workspace_output = knit(&workspace, ["bundle", "venue capacity", "--agents"]);
    assert!(existing_workspace_output.contains("AGENTS.md"));
    let existing_workspace_agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(existing_workspace_agents.contains("This is a Knit workspace"));

    fs::write(workspace.join("AGENTS.md"), "custom guidance\n").unwrap();
    knit(
        &workspace,
        ["bundle", "venue capacity", "--force", "--agents"],
    );
    let updated = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(updated.contains("custom guidance"));
    assert!(updated.contains("This is a Knit workspace"));
    assert_eq!(updated.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);

    knit(
        &workspace,
        ["bundle", "venue capacity", "--force", "--agents"],
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

    knit(&workspace, ["init", "demo"]);
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
    assert!(knit(&workspace, ["project", "list"]).contains("demo"));
    assert!(knit(&workspace, ["project", "show"]).contains("\"id\": \"demo\""));

    knit(&workspace, ["bundle", "project feature"]);

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
    assert_eq!(bundle["projectId"].as_str(), Some("demo"));
    assert_eq!(bundle["repos"].as_array().unwrap().len(), 2);
    assert_eq!(bundle["repos"][0]["id"].as_str(), Some("backend"));
    assert_eq!(bundle["repos"][1]["id"].as_str(), Some("frontend"));

    let list = knit(&workspace, ["bundle", "list"]);
    assert!(list.contains("project-feature"));

    fs::remove_dir_all(root).unwrap();
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
    knit(&workspace, ["bundle", "default feature"]);
    assert_eq!(
        bundle_repo_ids(&workspace, "default-feature"),
        vec!["backend"]
    );

    // Named view "frontend" => {frontend, docs}; ad-hoc --include adds backend.
    knit(
        &workspace,
        [
            "bundle",
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
fn view_save_accepts_comma_separated_exclude_list() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(
        &workspace,
        ["view", "save", "backend", "--exclude", "frontend,docs"],
    );

    let repos = knit(&workspace, ["view", "show", "backend", "--repos"]);
    assert_eq!(
        repos.lines().collect::<Vec<_>>(),
        vec!["backend"],
        "{repos}"
    );

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
        ["bundle", "x", "--view", "backend", "--repo", "backend"],
    );
    assert!(error.contains("not together with --repo"), "{error}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundle_include_and_exclude_manage_worktrees() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);

    knit(&workspace, ["bundle", "live feature"]);
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
    knit(&workspace, ["init", "demo"]);
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
        ["bundle", "project feature", "--agents", "--cd"],
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
    knit(&workspace, ["init", "demo"]);
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
        ["bundle", "project feature", "--cd", "frontend"],
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
fn project_remove_deletes_template_and_clears_active_project() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    let project_path = workspace.join(".knit/projects/demo.project.json");
    assert!(project_path.exists());
    let refused = knit_fails(&workspace, ["project", "remove", "demo"]);
    assert!(refused.contains("requires --force"));

    let removed = knit(&workspace, ["project", "remove", "demo", "--force"]);
    assert!(removed.contains("Removed project"));
    assert!(!project_path.exists());
    let config: Value =
        serde_json::from_str(&fs::read_to_string(workspace.join(".knit/config.json")).unwrap())
            .unwrap();
    assert!(config.get("activeProject").is_none());
    assert!(!knit(&workspace, ["project", "list"]).contains("demo"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cherrypick_moves_source_commits_into_destination_bundle() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );

    knit(
        &workspace,
        ["bundle", "source feature", "--repo", "backend"],
    );
    let source_checkout = workspace.join(".knit/worktrees/source-feature/backend");
    append_line(&source_checkout.join("app.txt"), "source change");
    git(&source_checkout, ["add", "app.txt"]);
    git(&source_checkout, ["commit", "-m", "Source change"]);
    let source_sha = git(&source_checkout, ["rev-parse", "HEAD"]);
    knit(&workspace, ["--bundle", "source-feature", "sync"]);

    knit(
        &workspace,
        ["bundle", "picked feature", "--repo", "backend"],
    );
    let picked = knit(
        &workspace,
        [
            "--bundle",
            "picked-feature",
            "cherrypick",
            "--from",
            "source-feature",
            source_sha.trim(),
        ],
    );
    assert!(picked.contains("picking"));

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
fn project_agents_are_generated_from_project_json() {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    fs::write(
        workspace.join("AGENTS.md"),
        "custom guidance\n\n## Demo Knit Project\n\nThat command should add all four Demo repos by default:\n\n- `backend`\n\n<!-- BEGIN GLOSS AGENTS -->\nkeep this\n<!-- END GLOSS AGENTS -->\n",
    )
    .unwrap();

    let refresh = knit(&workspace, ["init", "demo", "--agents"]);
    assert!(refresh.contains("Project AGENTS.md"));
    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("custom guidance"));
    assert!(agents.contains("<!-- BEGIN KNIT PROJECT AGENTS: demo -->"));
    assert!(agents.contains("That command adds these default repos from the project data:"));
    assert!(agents.contains("- `backend`"));
    assert!(!agents.contains("all four Demo repos"));
    assert!(agents.contains("<!-- BEGIN GLOSS AGENTS -->"));
    assert!(agents.contains("<!-- END KNIT PROJECT AGENTS: demo -->\n<!-- BEGIN GLOSS AGENTS -->"));

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
            .matches("<!-- BEGIN KNIT PROJECT AGENTS: demo -->")
            .count(),
        1
    );
    assert!(updated.contains("- `backend`"));
    assert!(updated.contains("- `frontend`"));

    let project_path = workspace.join(".knit/projects/demo.project.json");
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

    knit(&workspace, ["project", "agents", "demo"]);
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

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );

    let start = knit(&workspace, ["bundle", "agent docs"]);
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
    assert!(backend_agents.contains("knit commit --all"));
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
    knit(&workspace, ["bundle", "agent docs", "--agents"]);
    let updated = fs::read_to_string(&backend_agents_path).unwrap();
    assert!(updated.contains("repo guidance"));
    assert_eq!(updated.matches("<!-- BEGIN KNIT AGENTS -->").count(), 1);
    assert!(updated.contains("knit status"));
    assert!(updated.contains("knit push --set-upstream"));
    assert!(!updated.contains("knit --bundle"));
    assert!(workspace.join("AGENTS.md").exists());

    let second_start = knit(&workspace, ["bundle", "agent docs two"]);
    assert!(second_start.contains("Worktree AGENTS.md: 2 repo worktree(s)"));
    let second_agents =
        fs::read_to_string(workspace.join(".knit/worktrees/agent-docs-two/backend/AGENTS.md"))
            .unwrap();
    assert!(second_agents.contains("bundle `agent-docs-two`"));
    assert!(second_agents.contains("knit commit --all"));
    assert!(second_agents.contains("knit push --set-upstream"));
    assert!(!second_agents.contains("knit --bundle"));

    fs::remove_dir_all(root).unwrap();
}
