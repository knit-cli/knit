mod common;
use common::project_auth::{clone_args, export_body, read_json};
use common::*;
use serde_json::json;
use std::fs;

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
