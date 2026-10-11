mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const BUNDLE: &str = "venue-capacity";

/// Two-repo project whose landing merges backend before frontend, published
/// as `venue-capacity`.
fn published_project_bundle(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "github",
        "merge": { "repoOrder": ["backend", "frontend"], "method": "merge" }
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    for repo in ["backend", "frontend"] {
        append_line(
            &workspace.join(format!(".knit/worktrees/{BUNDLE}/{repo}/app.txt")),
            &format!("{repo} landed change"),
        );
    }
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
    for repo in bundle(&workspace)["repos"].as_array().unwrap() {
        let id = repo["id"].as_str().unwrap();
        fs::write(
            fake_gh_dir.join(format!("head-{id}")),
            repo["headSha"].as_str().unwrap(),
        )
        .unwrap();
        fs::write(
            fake_gh_dir.join(format!("clone-{id}")),
            root.join(format!("{id}-collaborator")).to_str().unwrap(),
        )
        .unwrap();
    }
    (workspace, fake_bin, fake_gh_dir)
}

/// What the forge does for a revert PR: a branch holding the revert commit,
/// pushed from the source clone.
fn push_forge_revert_branch(root: &Path, fake_gh_dir: &Path, repo: &str, feature_sha: &str) {
    let clone = root.join(format!("{repo}-collaborator"));
    git(&clone, ["fetch", "-q", "origin"]);
    git(
        &clone,
        [
            "checkout",
            "-q",
            "-b",
            &format!("knit/revert-{repo}"),
            "origin/main",
        ],
    );
    git(&clone, ["revert", "--no-edit", feature_sha]);
    git(
        &clone,
        ["push", "-q", "origin", &format!("knit/revert-{repo}")],
    );
    let head = git(&clone, ["rev-parse", "HEAD"]);
    git(&clone, ["checkout", "-q", "main"]);
    fs::write(fake_gh_dir.join(format!("revert-head-{repo}")), head.trim()).unwrap();
}

fn bundle(workspace: &Path) -> Value {
    serde_json::from_str(
        &fs::read_to_string(workspace.join(format!(".knit/bundles/{BUNDLE}.bundle.json"))).unwrap(),
    )
    .unwrap()
}

fn merge_order(fake_gh_dir: &Path) -> Vec<String> {
    fs::read_to_string(fake_gh_dir.join("merge-order.txt"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn merge_step_ids(plan: &Value) -> Vec<String> {
    plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["type"] == "merge_pr")
        .map(|step| step["id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn landed_bundle_is_reverted_and_relanded_in_reverse_order_without_restore() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = published_project_bundle(&root);
    let plan_path = workspace.join(format!(".knit/land-plans/{BUNDLE}.land.json"));

    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let forward: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    assert_eq!(
        merge_step_ids(&forward),
        vec!["merge-backend", "merge-frontend"]
    );
    knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert_eq!(merge_order(&fake_gh_dir), vec!["backend", "frontend"]);
    let landed = bundle(&workspace);
    assert_eq!(landed["state"], "archived");
    assert!(!workspace.join(format!(".knit/worktrees/{BUNDLE}")).exists());

    // Nothing resolves without a selector; the error names the landed bundle.
    let unselected = knit_fails(&workspace, ["revert", "landed"]);
    assert!(
        unselected.contains(&format!("--bundle {BUNDLE}")),
        "{unselected}"
    );

    // A commit group needs the removed checkouts; the error names the
    // command that works instead.
    let group_id = landed["commitGroups"][0]["id"].as_str().unwrap().to_owned();
    let local = knit_fails(&workspace, ["--bundle", BUNDLE, "revert", &group_id]);
    assert!(
        local.contains(&format!("knit --bundle {BUNDLE} revert landed --apply")),
        "{local}"
    );

    let preview = knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "revert", "landed"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(preview.contains("prRevert"), "{preview}");
    assert!(preview.contains("feature.landed"), "{preview}");
    assert!(
        preview.contains(&format!("knit --bundle {BUNDLE} revert landed --apply")),
        "{preview}"
    );
    assert!(!fake_gh_dir.join("revert-order.txt").exists());

    for repo in landed["repos"].as_array().unwrap() {
        push_forge_revert_branch(
            &root,
            &fake_gh_dir,
            repo["id"].as_str().unwrap(),
            repo["headSha"].as_str().unwrap(),
        );
    }

    // Apply works in one step even when no plan was written first.
    fs::remove_dir_all(workspace.join(".knit/revert-plans")).unwrap();
    let applied = knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "revert", "landed", "--apply"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(applied.contains("Recorded PR revert group"), "{applied}");
    assert!(applied.contains("Reopened bundle"), "{applied}");
    assert!(
        applied.contains(&format!("knit --bundle {BUNDLE} land apply")),
        "{applied}"
    );
    let reopened = bundle(&workspace);
    assert_eq!(reopened["state"], "open");
    assert!(reopened["archivedAt"].is_null());
    let latest = reopened["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["type"], "pr.revert");
    let urls = reopened["publications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|publication| publication["url"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(urls.contains(&"https://github.com/acme/backend/pull/901".to_owned()));
    assert!(urls.contains(&"https://github.com/acme/frontend/pull/902".to_owned()));
    assert!(!workspace.join(format!(".knit/worktrees/{BUNDLE}")).exists());
    for repo in reopened["repos"].as_array().unwrap() {
        let id = repo["id"].as_str().unwrap();
        let revert_head =
            fs::read_to_string(fake_gh_dir.join(format!("revert-head-{id}"))).unwrap();
        assert_eq!(repo["headSha"].as_str(), Some(revert_head.trim()));
    }

    // The landing plan of the original merge is replaced by the reverts,
    // merged in reverse order.
    knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land"],
        &fake_bin,
        &fake_gh_dir,
    );
    let reverse: Value = serde_json::from_str(&fs::read_to_string(&plan_path).unwrap()).unwrap();
    assert_eq!(
        merge_step_ids(&reverse),
        vec!["merge-frontend", "merge-backend"]
    );
    let backend_step = reverse["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["id"] == "merge-backend")
        .unwrap();
    assert_eq!(backend_step["needs"], json!(["merge-frontend"]));

    let validated = knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land", "validate"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(validated.contains("\"valid\":true"), "{validated}");

    knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert_eq!(
        merge_order(&fake_gh_dir),
        vec!["backend", "frontend", "frontend", "backend"]
    );
    let relanded = bundle(&workspace);
    assert_eq!(relanded["state"], "archived");
    let landings = relanded["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "feature.landed")
        .count();
    assert_eq!(landings, 2);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn landed_selector_requires_a_recorded_landing() {
    let root = unique_temp_dir();
    let (workspace, _fake_bin, _fake_gh_dir) = published_project_bundle(&root);

    let error = knit_fails(&workspace, ["revert", "landed"]);
    assert!(error.contains("no recorded landing"), "{error}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_validate_defaults_to_the_bundle_plan() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = published_project_bundle(&root);

    let missing = knit_fails(&workspace, ["land", "validate"]);
    assert!(missing.contains("No land plan found"), "{missing}");
    assert!(
        missing.contains(&format!("{BUNDLE}.land.json")),
        "{missing}"
    );

    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let valid = knit(&workspace, ["land", "validate", "--json"]);
    assert!(valid.contains("\"valid\":true"), "{valid}");
    let explicit = knit(
        &workspace,
        [
            "land",
            "validate",
            "--plan",
            workspace
                .join(format!(".knit/land-plans/{BUNDLE}.land.json"))
                .to_str()
                .unwrap(),
        ],
    );
    assert!(explicit.contains("\"valid\":true"), "{explicit}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn publish_names_sync_when_commits_were_made_outside_knit() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "outside commit"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/outside-commit/backend");
    append_line(&feature.join("app.txt"), "plain git change");
    configure_git_user(&feature);
    git(&feature, ["commit", "-am", "Plain git change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    let error = knit_fails_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(error.contains("no recorded commits") || error.contains("have recorded commits"));
    assert!(error.contains("backend"), "{error}");
    assert!(
        error.contains("knit --bundle outside-commit sync"),
        "{error}"
    );

    fs::remove_dir_all(root).unwrap();
}
