mod common;
use common::project_auth::{clone_args, export_body, read_json};
use common::*;
use serde_json::json;
use std::{fs, process::Command};

#[test]
fn incremental_pull_preserves_legacy_drafts_on_add_and_checkout_recovery() {
    let root = unique_temp_dir();
    let backend = root.join("backend-source");
    let library = root.join("library-source");
    init_repo(&backend, "backend");
    init_repo(&library, "library");
    let fake = root.join("remote");
    let base = spawn_fake_remote_api(
        &fake,
        export_body(
            &[("backend".into(), backend.to_string_lossy().into_owned())],
            None,
        )
        .to_string(),
    );
    let workspace = root.join("workspace");
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let env = [("KNIT_HOME", home.to_str().unwrap())];
    knit_with_env(&root, clone_args(&workspace, &base, &[]), &env);
    let mut export = export_body(
        &[
            ("backend".into(), backend.to_string_lossy().into_owned()),
            ("library".into(), library.to_string_lossy().into_owned()),
        ],
        None,
    );
    export["data"]["knitProject"]["repos"][1]["publish"] = json!({"draft":true});
    fs::write(fake.join("export.json"), export.to_string()).unwrap();
    let added = knit_with_env(&workspace, ["pull", "--bundles"], &env);
    assert!(added.contains("added"), "{added}");
    let assert_draft = || {
        let project = read_json(&workspace.join(".knit/projects/demo.project.json"));
        let repo = project["repos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "library")
            .unwrap();
        assert_eq!(repo["publish"], json!({"draft":true}));
    };
    assert_draft();
    fs::remove_dir_all(workspace.join("library")).unwrap();
    let recovered = knit_with_env(&workspace, ["pull", "--bundles"], &env);
    assert!(recovered.contains("recovered"), "{recovered}");
    assert!(workspace.join("library/.git").exists());
    assert_draft();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bare_relative_artifact_uses_existing_directory_for_gh_and_glab_template_lookup() {
    for (cli, host) in [("gh", "github.com"), ("glab", "gitlab.com")] {
        let root = unique_temp_dir();
        let bin = root.join("bin");
        let log = root.join("calls.jsonl");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        write_fake_python_cli(
            &bin,
            cli,
            r#"import json, os, sys
with open(os.environ['TEMPLATE_LOG'], 'a') as f:
    f.write(json.dumps({'cwd':os.getcwd(), 'args':sys.argv[1:]})+'\n')
if any('docs' in a and 'PULL_REQUEST_TEMPLATE.md' in a for a in sys.argv):
    print('## Template\nTitle: Authored template prose\nChecklist')
else:
    print('HTTP 404 missing template',file=sys.stderr)
    sys.exit(1)
"#,
        );
        let artifact = json!({"schemaVersion":"0.1","kind":"ChangeGroup","id":"template","title":"Template review","createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","repos":[{"id":"library","path":".","remote":format!("https://{host}/example/library.git"),"baseBranch":"main","featureBranch":"knit/template"}],"commitGroups":[],"nodes":[],"publish":{"body":{"fallback":"upstream-template"}}});
        fs::write(root.join("bundle.json"), artifact.to_string()).unwrap();
        let before = fs::read(root.join("bundle.json")).unwrap();
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_knit"))
            .current_dir(&root)
            .args([
                "publish",
                "create",
                "--from-artifact",
                "bundle.json",
                "--all",
                "--dry-run",
            ])
            .env("PATH", path)
            .env("TEMPLATE_LOG", &log)
            .env("KNIT_HOME", home)
            .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
            .env_remove("KNIT_BUNDLE")
            .env_remove("KNIT_SESSION")
            .env_remove("KNIT_GITHUB_API_TRANSPORT")
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&output.stdout);
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{cli}: {out}\n{err}");
        assert!(out.contains("Title: Authored template prose"));
        assert!(
            out.contains(
                "body source: upstream:example/library/docs/PULL_REQUEST_TEMPLATE.md@main"
            ),
            "{out}"
        );
        let calls = fs::read_to_string(log).unwrap();
        assert_eq!(calls.lines().count(), 4, "{calls}");
        for line in calls.lines() {
            let call: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(
                fs::canonicalize(call["cwd"].as_str().unwrap()).unwrap(),
                fs::canonicalize(&root).unwrap()
            );
        }
        assert_eq!(fs::read(root.join("bundle.json")).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }
}
