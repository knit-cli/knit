mod common;
use common::*;
use serde_json::{json, Value};
use std::{fs, process::Command};

#[test]
fn configured_dry_run_is_read_only_and_matches_created_content() {
    let root = unique_temp_dir();
    let (remote, source, _) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "configured publishing"]);
    knit(&workspace, ["bundle", "add", source.to_str().unwrap()]);
    let checkout = workspace.join(".knit/worktrees/configured-publishing/backend");
    append_line(&checkout.join("app.txt"), "publish feature");
    knit(
        &workspace,
        ["commit", "--all", "-m", "Configured publishing"],
    );
    let path = workspace.join(".knit/bundles/configured-publishing.bundle.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    bundle["publish"] = json!({"draft":"all","title":"file","body":{"file":"PR-{repo}.md","fallback":"knit"},"future":{"keep":true}});
    fs::write(&path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
    fs::write(
        checkout.parent().unwrap().join("PR-backend.md"),
        "Title: File review title\nWritten by the author.\n",
    )
    .unwrap();
    let bin = root.join("bin");
    let forge = root.join("forge");
    write_fake_gh(&bin, &forge);
    let before = fs::read(&path).unwrap();
    let preview = knit_with_fake_gh(
        &workspace,
        [
            "publish",
            "create",
            "--github",
            "--dry-run",
            "--ready",
            "backend",
            "--no-remote",
        ],
        &bin,
        &forge,
    );
    assert!(preview.contains("draft=false"), "{preview}");
    assert!(preview.contains("File review title"));
    assert!(preview.contains("Written by the author."));
    assert!(preview.contains("body source: file:"));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!forge.join("create-backend.md").exists());
    assert!(!Command::new("git")
        .current_dir(&remote)
        .args([
            "show-ref",
            "--verify",
            "refs/heads/knit/configured-publishing"
        ])
        .output()
        .unwrap()
        .status
        .success());
    knit_with_fake_gh(
        &workspace,
        [
            "publish",
            "create",
            "--github",
            "--ready",
            "backend",
            "--no-sync",
            "--no-remote",
        ],
        &bin,
        &forge,
    );
    let args = fs::read_to_string(forge.join("create-backend.args")).unwrap();
    assert!(args.contains("File review title"), "{args}");
    assert!(!args.contains("--draft"), "{args}");
    let body = fs::read_to_string(forge.join("create-backend.md")).unwrap();
    assert!(body.starts_with("Written by the author.\n"));
    assert!(!body.contains("Title:"));
    assert!(body.contains("<!-- BEGIN KNIT BUNDLE -->"));
    let after: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(after["publish"]["future"]["keep"], true);
}

#[test]
fn project_pull_clears_removed_and_null_publish_policy() {
    let root = unique_temp_dir();
    let repo = root.join("stack");
    init_repo(&repo, "stack");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "example"]);
    knit(
        &workspace,
        ["project", "add", "stack", repo.to_str().unwrap()],
    );
    let path = workspace.join(".knit/projects/example.project.json");
    let mut source: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for explicit_null in [false, true] {
        source["publish"] = json!({"draft":"all","future":{"keep":true}});
        fs::write(
            repo.join("knit.project.json"),
            serde_json::to_vec(&source).unwrap(),
        )
        .unwrap();
        knit(&workspace, ["project", "pull", "--repo", "stack"]);
        let loaded: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(loaded["publish"]["future"]["keep"], true);
        source.as_object_mut().unwrap().remove("publish");
        if explicit_null {
            source["publish"] = Value::Null;
        }
        fs::write(
            repo.join("knit.project.json"),
            serde_json::to_vec(&source).unwrap(),
        )
        .unwrap();
        knit(&workspace, ["project", "pull", "--repo", "stack"]);
        let loaded: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(loaded.get("publish").is_none());
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn publish_rejects_pre_pushed_foreign_commits_and_respects_effective_target() {
    for lane in [false, true] {
        let root = unique_temp_dir();
        let (_, source, _) = init_remote_repo(&root, "backend");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        knit(&workspace, ["init", "example"]);
        knit(
            &workspace,
            ["project", "add", "backend", source.to_str().unwrap()],
        );
        knit(&workspace, ["bundle", "author check"]);
        let checkout = workspace.join(".knit/worktrees/author-check/backend");
        append_line(&checkout.join("app.txt"), "foreign change");
        git(&checkout, ["add", "app.txt"]);
        git(
            &checkout,
            [
                "commit",
                "--author",
                "Foreign Author <foreign@example.test>",
                "-m",
                "Foreign change",
            ],
        );
        knit(&workspace, ["sync"]);
        git(
            &checkout,
            ["push", "origin", "HEAD:refs/heads/knit/author-check"],
        );
        let bin = root.join("bin");
        let forge = root.join("forge");
        write_fake_gh(&bin, &forge);
        let bundle_path = workspace.join(".knit/bundles/author-check.bundle.json");
        let before = fs::read(&bundle_path).unwrap();
        let err = knit_fails_with_fake_gh(
            &workspace,
            ["publish", "create", "--no-sync", "--no-remote"],
            &bin,
            &forge,
        );
        assert!(err.contains("foreign@example.test"), "{err}");
        assert!(!forge.join("create-backend.args").exists());
        assert_eq!(fs::read(&bundle_path).unwrap(), before);
        // A target already containing that commit excludes it from this review.
        git(&checkout, ["push", "origin", "HEAD:refs/heads/release"]);
        append_line(&checkout.join("app.txt"), "own change");
        knit(&workspace, ["commit", "--all", "-m", "Own change"]);
        let flag = if lane {
            let path = workspace.join(".knit/projects/example.project.json");
            let mut p: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            p["landing"] = json!({"lanes":{"release":{"defaultBranch":"release"}}});
            fs::write(path, serde_json::to_vec(&p).unwrap()).unwrap();
            "--lane"
        } else {
            "--target"
        };
        knit_with_fake_gh(
            &workspace,
            [
                "publish",
                "create",
                flag,
                "release",
                "--no-sync",
                "--no-remote",
            ],
            &bin,
            &forge,
        );
        assert_eq!(
            fs::read_to_string(forge.join("create-backend.base"))
                .unwrap()
                .trim(),
            "release"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn consumer_only_preview_refreshes_merged_library_without_mutating_bundle() {
    let root = unique_temp_dir();
    let (_, backend, _) = init_remote_repo(&root, "backend");
    let (_, frontend, _) = init_remote_repo(&root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "example"]);
    for (id, path) in [("backend", &backend), ("frontend", &frontend)] {
        knit(&workspace, ["project", "add", id, path.to_str().unwrap()]);
    }
    knit(&workspace, ["bundle", "live dependencies"]);
    for id in ["backend", "frontend"] {
        append_line(
            &workspace
                .join(".knit/worktrees/live-dependencies")
                .join(id)
                .join("app.txt"),
            "change",
        );
    }
    knit(&workspace, ["commit", "--all", "-m", "Feature"]);
    let project_path = workspace.join(".knit/projects/example.project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    project["publish"] = json!({"draft":"dependents"});
    project["landing"] = json!({"dependencies":[{"library":"backend","consumers":["frontend"]}]});
    fs::write(&project_path, serde_json::to_vec(&project).unwrap()).unwrap();
    write_bundle_publications_for_repos(&workspace, "live-dependencies", "OPEN", &["backend"]);
    let bundle_path = workspace.join(".knit/bundles/live-dependencies.bundle.json");
    let before = fs::read(&bundle_path).unwrap();
    let bin = root.join("bin");
    let forge = root.join("forge");
    write_fake_gh(&bin, &forge);
    let args = ["publish", "create", "frontend", "--dry-run", "--no-remote"];
    let open = knit_with_fake_gh(&workspace, args, &bin, &forge);
    assert!(open.contains("draft=true"), "{open}");
    assert!(
        open.contains("Blocked on [backend #1](https://github.com/acme/backend/pull/1)"),
        "{open}"
    );
    fs::write(forge.join("merged-backend"), "").unwrap();
    let merged = knit_with_fake_gh(&workspace, args, &bin, &forge);
    assert!(merged.contains("draft=false"), "{merged}");
    assert!(!merged.contains("Blocked on"), "{merged}");
    assert_eq!(fs::read(&bundle_path).unwrap(), before);
    project["landing"]["dependencies"][0]["release"] = json!({"instructions":"Publish release"});
    fs::write(&project_path, serde_json::to_vec(&project).unwrap()).unwrap();
    let unreleased = knit_with_fake_gh(&workspace, args, &bin, &forge);
    assert!(unreleased.contains("draft=true"), "{unreleased}");
    assert_eq!(fs::read(&bundle_path).unwrap(), before);
    assert!(!forge.join("create-frontend.args").exists());
    project["landing"]["dependencies"][0]
        .as_object_mut()
        .unwrap()
        .remove("release");
    fs::write(&project_path, serde_json::to_vec(&project).unwrap()).unwrap();
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "frontend", "--no-sync", "--no-remote"],
        &bin,
        &forge,
    );
    assert!(!fs::read_to_string(forge.join("create-frontend.args"))
        .unwrap()
        .contains("--draft"));
    assert!(!fs::read_to_string(forge.join("create-frontend.md"))
        .unwrap()
        .contains("Blocked on"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sync_applies_changed_body_files_and_keeps_host_edits() {
    let root = unique_temp_dir();
    let (_remote, source, _) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "retitled review"]);
    knit(&workspace, ["bundle", "add", source.to_str().unwrap()]);
    let checkout = workspace.join(".knit/worktrees/retitled-review/backend");
    append_line(&checkout.join("app.txt"), "retitle feature");
    knit(&workspace, ["commit", "--all", "-m", "Retitled review"]);
    let bin = root.join("bin");
    let forge = root.join("forge");
    write_fake_gh(&bin, &forge);
    let publish = ["publish", "create", "--github", "--no-sync", "--no-remote"];
    knit_with_fake_gh(&workspace, publish, &bin, &forge);
    let args = fs::read_to_string(forge.join("create-backend.args")).unwrap();
    assert!(args.contains("retitled review (backend)"), "{args}");
    let sync = ["publish", "sync", "--github"];
    knit_with_fake_gh(&workspace, sync, &bin, &forge);
    assert!(!forge.join("edit-backend.title").exists());

    let file = checkout.parent().unwrap().join("PR-backend.md");
    fs::write(
        &file,
        "Title: Precise review title\n\nWhat changed and why.\n",
    )
    .unwrap();
    knit_with_fake_gh(&workspace, sync, &bin, &forge);
    let title = fs::read_to_string(forge.join("edit-backend.title")).unwrap();
    assert_eq!(title.trim(), "Precise review title");
    let body = fs::read_to_string(forge.join("edit-backend.md")).unwrap();
    assert!(body.starts_with("\nWhat changed and why.\n\n"), "{body}");
    assert!(!body.contains("Existing body"), "{body}");
    assert!(body.contains("<!-- BEGIN KNIT BUNDLE -->"), "{body}");
    let path = workspace.join(".knit/bundles/retitled-review.bundle.json");
    let bundle: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(bundle["publications"][0]["title"], "Precise review title");
    assert_eq!(
        bundle["publications"][0]["applied"]["title"],
        "Precise review title"
    );

    // The host still reports its own title and text; with the file unchanged
    // a sync leaves them alone.
    fs::remove_file(forge.join("edit-backend.title")).unwrap();
    knit_with_fake_gh(&workspace, sync, &bin, &forge);
    assert!(!forge.join("edit-backend.title").exists());
    let body = fs::read_to_string(forge.join("edit-backend.md")).unwrap();
    assert!(body.starts_with("Existing body"), "{body}");

    fs::write(
        &file,
        "Title: Sharper review title\n\nWhat changed and why.\n",
    )
    .unwrap();
    knit_with_fake_gh(&workspace, sync, &bin, &forge);
    let title = fs::read_to_string(forge.join("edit-backend.title")).unwrap();
    assert_eq!(title.trim(), "Sharper review title");
}
