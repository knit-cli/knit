mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;

#[test]
fn configured_preview_is_read_only_and_matches_mixed_review_creation() {
    let root = unique_temp_dir();
    let (backend_remote, backend, _) = init_remote_repo(&root, "backend");
    let (frontend_remote, frontend, _) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "preview"]);
    for (id, path) in [("backend", &backend), ("frontend", &frontend)] {
        knit(&workspace, ["project", "add", id, path.to_str().unwrap()]);
    }
    knit(&workspace, ["bundle", "policy cli"]);
    let bundle_root = workspace.join(".knit/worktrees/policy-cli");
    for id in ["backend", "frontend"] {
        append_line(&bundle_root.join(id).join("app.txt"), "feature");
        fs::write(
            bundle_root.join(format!("PR-{id}.md")),
            format!("Title: Review {id}\n\nAuthor prose for {id}.\n"),
        )
        .unwrap();
    }
    knit(&workspace, ["commit", "--all", "-m", "Add shared API"]);
    let project_path = workspace.join(".knit/projects/preview.project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    project["publish"] =
        json!({"draft":"dependents","title":"file","body":{"file":"PR-{repo}.md"}});
    project["landing"] = json!({"dependencies":[{"library":"backend","consumers":["frontend"]}]});
    fs::write(&project_path, serde_json::to_vec_pretty(&project).unwrap()).unwrap();
    let bundle_path = workspace.join(".knit/bundles/policy-cli.bundle.json");
    let before = fs::read(&bundle_path).unwrap();
    let fake_bin = root.join("fake-bin");
    let fake_gh = root.join("fake-gh");
    write_fake_gh(&fake_bin, &fake_gh);
    let preview = knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--dry-run"],
        &fake_bin,
        &fake_gh,
    );
    assert_eq!(fs::read(&bundle_path).unwrap(), before);
    for (id, remote) in [("backend", &backend_remote), ("frontend", &frontend_remote)] {
        assert!(!git_success(
            remote,
            ["show-ref", "--verify", "refs/heads/knit/policy-cli"]
        ));
        assert!(!fake_gh.join(format!("create-{id}.args")).exists());
        assert!(
            preview.contains(&format!("title: Review {id}")),
            "{preview}"
        );
        assert!(preview.contains(&format!("PR-{id}.md")), "{preview}");
    }
    assert!(
        preview
            .lines()
            .any(|line| line.starts_with("backend:") && line.contains("draft=false")),
        "{preview}"
    );
    assert!(
        preview
            .lines()
            .any(|line| line.starts_with("frontend:") && line.contains("draft=true")),
        "{preview}"
    );
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--no-sync", "--no-remote"],
        &fake_bin,
        &fake_gh,
    );
    let backend_args = fs::read_to_string(fake_gh.join("create-backend.args")).unwrap();
    let frontend_args = fs::read_to_string(fake_gh.join("create-frontend.args")).unwrap();
    assert!(!backend_args.contains("--draft"), "{backend_args}");
    assert!(frontend_args.contains("--draft"), "{frontend_args}");
    for id in ["backend", "frontend"] {
        let args = fs::read_to_string(fake_gh.join(format!("create-{id}.args"))).unwrap();
        assert!(args.contains(&format!("--title Review {id}")), "{args}");
        let body = fs::read_to_string(fake_gh.join(format!("create-{id}.md"))).unwrap();
        assert!(body.contains(&format!("Author prose for {id}.")), "{body}");
        assert!(!body.contains("Title:"), "{body}");
        assert!(body.contains("<!-- BEGIN KNIT BUNDLE -->"), "{body}");
    }
    let consumer = fs::read_to_string(fake_gh.join("create-frontend.md")).unwrap();
    assert!(
        consumer.contains("Blocked on [backend #101](https://github.com/acme/backend/pull/101)"),
        "{consumer}"
    );
    let authored = "\nAuthor prose for frontend.\n";
    let (prefix, block) = consumer.split_once("<!-- BEGIN KNIT BUNDLE -->").unwrap();
    assert_eq!(prefix, format!("{authored}\n\n"));
    assert!(block.contains("Blocked on [backend #101](https://github.com/acme/backend/pull/101)"));

    // Return the created body on view so the real sync command replaces its
    // managed block while preserving authored whitespace and trailing prose.
    let current_body = format!("{consumer}\n\nAuthor tail  \nBlocked on backend\n");
    fs::write(
        fake_gh.join("current-frontend.json"),
        serde_json::to_vec(&json!({
            "number":202,"url":"https://github.com/acme/frontend/pull/202",
            "state":"OPEN","baseRefName":"main","headRefName":"knit/policy-cli",
            "body":current_body
        }))
        .unwrap(),
    )
    .unwrap();
    let gh_path = fake_bin.join("gh");
    let gh = fs::read_to_string(&gh_path).unwrap();
    let (shebang, script) = gh.split_once('\n').unwrap();
    fs::write(
        &gh_path,
        format!(
            r#"{shebang}
if [ "$1" = pr ] && [ "$2" = view ] && [ "$3" = https://github.com/acme/frontend/pull/202 ]; then
  cat "$GH_FAKE_DIR/current-frontend.json"
  exit 0
fi
{script}"#
        ),
    )
    .unwrap();
    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    let library = bundle["publications"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["repoId"] == "backend")
        .unwrap();
    library["number"] = json!(102);
    library["url"] = json!("https://github.com/acme/backend/pull/102");
    fs::write(&bundle_path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    knit_with_fake_gh(&workspace, ["publish", "sync"], &fake_bin, &fake_gh);
    let synced = fs::read_to_string(fake_gh.join("edit-frontend.md")).unwrap();
    assert_eq!(
        synced,
        current_body
            .replace("#101", "#102")
            .replace("/pull/101", "/pull/102")
            .replace(
                "- `frontend`: pending",
                "- `frontend`: https://github.com/acme/frontend/pull/202 (this PR)",
            )
    );
    let (_, block) = synced.split_once("<!-- BEGIN KNIT BUNDLE -->").unwrap();
    let (block, _) = block.split_once("<!-- END KNIT BUNDLE -->").unwrap();
    assert!(block.contains("Blocked on [backend #102](https://github.com/acme/backend/pull/102)"));
    fs::remove_dir_all(root).unwrap();
}
