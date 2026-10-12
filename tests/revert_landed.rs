mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const BUNDLE: &str = "venue-capacity";

// Landing ownership locks live in TMPDIR and are keyed by project, and every
// test here uses project `demo`; a private TMPDIR keeps parallel tests apart.
fn private_tmp(fake_bin: &Path) -> String {
    let dir = fake_bin.parent().unwrap().join("tmp");
    fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

fn knit_with_fake_gh<I, S>(cwd: &Path, args: I, fake_bin: &Path, fake_gh_dir: &Path) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let tmp = private_tmp(fake_bin);
    knit_with_fake_gh_env(cwd, args, fake_bin, fake_gh_dir, &[("TMPDIR", &tmp)])
}

fn knit_fails_with_fake_gh<I, S>(cwd: &Path, args: I, fake_bin: &Path, fake_gh_dir: &Path) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let tmp = private_tmp(fake_bin);
    knit_fails_with_fake_gh_env(cwd, args, fake_bin, fake_gh_dir, &[("TMPDIR", &tmp)])
}

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

    let relanding = knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        relanding.contains("Revert verified: venue capacity is undone in all 2 repositories."),
        "{relanding}"
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

fn land_forward(workspace: &Path, fake_bin: &Path, fake_gh_dir: &Path) {
    knit_with_fake_gh(workspace, ["land"], fake_bin, fake_gh_dir);
    knit_with_fake_gh(
        workspace,
        ["land", "apply", "--no-remote"],
        fake_bin,
        fake_gh_dir,
    );
    assert_eq!(bundle(workspace)["state"], "archived");
}

/// Prepare the forge's revert branches for every landed repository.
fn prepare_forge_reverts(root: &Path, workspace: &Path, fake_gh_dir: &Path) {
    for repo in bundle(workspace)["repos"].as_array().unwrap() {
        push_forge_revert_branch(
            root,
            fake_gh_dir,
            repo["id"].as_str().unwrap(),
            repo["headSha"].as_str().unwrap(),
        );
    }
}

fn commit_on_main(root: &Path, repo: &str, edit: impl FnOnce(&Path), message: &str) {
    let clone = root.join(format!("{repo}-collaborator"));
    git(&clone, ["checkout", "-q", "main"]);
    git(&clone, ["pull", "-q", "--ff-only", "origin", "main"]);
    edit(&clone);
    git(&clone, ["add", "-A"]);
    git(&clone, ["commit", "-q", "-m", message]);
    git(&clone, ["push", "-q", "origin", "main"]);
}

fn latest_landing(workspace: &Path) -> Value {
    bundle(workspace)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|node| node["type"] == "feature.landed")
        .unwrap()
        .clone()
}

#[test]
fn revert_landing_verifies_with_later_commits_on_the_base() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = published_project_bundle(&root);
    land_forward(&workspace, &fake_bin, &fake_gh_dir);

    commit_on_main(
        &root,
        "backend",
        |clone| fs::write(clone.join("other.txt"), "later work\n").unwrap(),
        "Later work in another file",
    );
    commit_on_main(
        &root,
        "frontend",
        |clone| {
            let path = clone.join("app.txt");
            let content = fs::read_to_string(&path).unwrap();
            fs::write(&path, format!("later header\n{content}")).unwrap();
        },
        "Later work in the same file",
    );

    prepare_forge_reverts(&root, &workspace, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "revert", "landed", "--apply"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land"],
        &fake_bin,
        &fake_gh_dir,
    );
    let landed = knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        landed.contains("backend: revert verified on main — exact inverse of PR #101 (1 file)"),
        "{landed}"
    );
    assert!(
        landed.contains("frontend: revert verified on main — exact inverse of PR #202 (1 file)"),
        "{landed}"
    );
    assert!(
        landed.contains("Revert verified: venue capacity is undone in all 2 repositories."),
        "{landed}"
    );
    assert_eq!(bundle(&workspace)["state"], "archived");
    assert_eq!(
        latest_landing(&workspace)["revertVerification"],
        json!({"verified": true})
    );

    // The later work is still there; only the bundle's change is gone.
    let frontend = root.join("frontend-collaborator");
    git(&frontend, ["pull", "-q", "--ff-only", "origin", "main"]);
    let content = fs::read_to_string(frontend.join("app.txt")).unwrap();
    assert!(content.starts_with("later header\n"), "{content}");
    assert!(!content.contains("frontend landed change"), "{content}");

    let status = knit(&workspace, ["--bundle", BUNDLE, "status"]);
    assert!(status.contains("revert verified"), "{status}");
    let log = knit(&workspace, ["--bundle", BUNDLE, "log"]);
    assert!(log.contains("revert verified"), "{log}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn edited_revert_fails_verification_and_keeps_the_bundle_open() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = published_project_bundle(&root);
    land_forward(&workspace, &fake_bin, &fake_gh_dir);
    prepare_forge_reverts(&root, &workspace, &fake_gh_dir);

    // The backend revert carries an unrelated edit, as a hand-resolved or
    // edited revert PR would.
    let clone = root.join("backend-collaborator");
    git(&clone, ["checkout", "-q", "knit/revert-backend"]);
    append_line(&clone.join("app.txt"), "hand edit");
    git(&clone, ["commit", "-q", "-a", "--amend", "--no-edit"]);
    git(
        &clone,
        ["push", "-q", "--force", "origin", "knit/revert-backend"],
    );
    let head = git(&clone, ["rev-parse", "HEAD"]);
    git(&clone, ["checkout", "-q", "main"]);
    fs::write(fake_gh_dir.join("revert-head-backend"), head.trim()).unwrap();

    knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "revert", "landed", "--apply"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land"],
        &fake_bin,
        &fake_gh_dir,
    );
    let failed = knit_fails_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        failed.contains(
            "backend: revert does not match on main — not the exact inverse of PR #101 in:"
        ),
        "{failed}"
    );
    assert!(failed.contains("  app.txt"), "{failed}");
    assert!(
        failed.contains("frontend: revert verified on main"),
        "{failed}"
    );
    assert!(failed.contains("was not archived"), "{failed}");
    assert!(!failed.contains("Revert verified:"), "{failed}");

    let after = bundle(&workspace);
    assert_ne!(after["state"], "archived");
    assert_eq!(
        latest_landing(&workspace)["revertVerification"],
        json!({"verified": false, "mismatched": {"backend": ["app.txt"]}})
    );
    let status = knit(&workspace, ["--bundle", BUNDLE, "status"]);
    assert!(status.contains("revert NOT verified"), "{status}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn revert_landing_runs_deployments_after_their_reversed_merges() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = published_project_bundle(&root);
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    let log = fake_gh_dir.join("merge-order.txt");
    project["landing"]["deployments"] = json!(["backend", "frontend"]
        .iter()
        .map(|repo| json!({
            "id": format!("deploy-{repo}"),
            "repoId": repo,
            "command": ["sh", "-c", format!("echo deploy-{repo} >> '{}'", log.display())]
        }))
        .collect::<Vec<_>>());
    fs::write(
        &project_path,
        serde_json::to_string_pretty(&project).unwrap(),
    )
    .unwrap();
    land_forward(&workspace, &fake_bin, &fake_gh_dir);
    let forward_entries = merge_order(&fake_gh_dir).len();

    prepare_forge_reverts(&root, &workspace, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "revert", "landed", "--apply"],
        &fake_bin,
        &fake_gh_dir,
    );
    knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land"],
        &fake_bin,
        &fake_gh_dir,
    );
    let plan: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(format!(".knit/land-plans/{BUNDLE}.land.json")))
            .unwrap(),
    )
    .unwrap();
    let step = |id: &str| {
        plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from {plan}"))
            .clone()
    };
    assert_eq!(step("deploy-backend")["needs"], json!(["merge-backend"]));
    assert_eq!(step("deploy-frontend")["needs"], json!(["merge-frontend"]));
    assert_eq!(step("merge-backend")["needs"], json!(["merge-frontend"]));

    let landed = knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(landed.contains("Revert verified:"), "{landed}");
    let reverse = merge_order(&fake_gh_dir)[forward_entries..].to_vec();
    let at = |entry: &str| {
        reverse
            .iter()
            .position(|line| line == entry)
            .unwrap_or_else(|| panic!("{entry} missing from {reverse:?}"))
    };
    assert_eq!(reverse.len(), 4, "{reverse:?}");
    assert!(at("frontend") < at("backend"), "{reverse:?}");
    assert!(at("frontend") < at("deploy-frontend"), "{reverse:?}");
    assert!(at("backend") < at("deploy-backend"), "{reverse:?}");
    assert_eq!(bundle(&workspace)["state"], "archived");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn revert_landed_apply_land_undoes_the_bundle_in_one_command() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = published_project_bundle(&root);
    land_forward(&workspace, &fake_bin, &fake_gh_dir);
    prepare_forge_reverts(&root, &workspace, &fake_gh_dir);

    let undone = knit_with_fake_gh(
        &workspace,
        ["--bundle", BUNDLE, "revert", "landed", "--apply", "--land"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(undone.contains("Recorded PR revert group"), "{undone}");
    assert!(!undone.contains("Next: run `knit --bundle"), "{undone}");
    assert!(
        undone.contains("Revert verified: venue capacity is undone in all 2 repositories."),
        "{undone}"
    );
    assert_eq!(
        merge_order(&fake_gh_dir),
        vec!["backend", "frontend", "frontend", "backend"]
    );
    assert_eq!(bundle(&workspace)["state"], "archived");
    assert_eq!(
        latest_landing(&workspace)["revertVerification"]["verified"],
        json!(true)
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_flag_requires_landed_target_and_apply() {
    let root = unique_temp_dir();
    let (workspace, _fake_bin, _fake_gh_dir) = published_project_bundle(&root);

    for args in [
        vec!["--bundle", BUNDLE, "revert", "landed", "--land"],
        vec!["--bundle", BUNDLE, "revert", "HEAD", "--apply", "--land"],
    ] {
        let error = knit_fails(&workspace, &args);
        assert!(
            error.contains("needs the `landed` target and `--apply`"),
            "{error}"
        );
        assert!(
            error.contains(&format!(
                "knit --bundle {BUNDLE} revert landed --apply --land"
            )),
            "{error}"
        );
    }
    assert!(!workspace.join(".knit/revert-plans").exists());

    fs::remove_dir_all(root).unwrap();
}
