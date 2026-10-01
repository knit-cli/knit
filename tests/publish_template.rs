mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;

#[test]
fn template_preview_reads_recorded_base_only_when_requested() {
    let root = unique_temp_dir();
    let (_, source, _) = init_remote_repo(&root, "backend");
    fs::create_dir_all(source.join(".github")).unwrap();
    fs::write(
        source.join(".github/pull_request_template.md"),
        "## Review checklist\nTitle: Ordinary template prose\n- [ ] Verify behavior\n",
    )
    .unwrap();
    git(&source, ["add", "."]);
    git(&source, ["commit", "-m", "Add review template"]);
    git(&source, ["push", "origin", "main"]);
    let base = git(&source, ["rev-parse", "HEAD"]);
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "template preview"]);
    knit(&workspace, ["bundle", "add", source.to_str().unwrap()]);
    let checkout = workspace.join(".knit/worktrees/template-preview/backend");
    fs::write(
        checkout.join(".github/pull_request_template.md"),
        "Feature template",
    )
    .unwrap();
    knit(&workspace, ["commit", "--all", "-m", "Review feature"]);
    let artifact = workspace.join(".knit/bundles/template-preview.bundle.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&artifact).unwrap()).unwrap();
    bundle["repos"][0]["remote"] = json!("https://github.com/example/backend.git");
    for fallback in [Some("upstream-template"), Some("knit"), None] {
        bundle["publish"] = json!({"body":{"file":"missing-{repo}.md"}});
        if let Some(fallback) = fallback {
            bundle["publish"]["body"]["fallback"] = json!(fallback);
        }
        fs::write(&artifact, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
        let before = fs::read(&artifact).unwrap();
        let preview = knit(&workspace, ["publish", "create", "--dry-run"]);
        if fallback == Some("upstream-template") {
            assert!(
                preview.contains(&format!(
                    "body source: upstream:.github/pull_request_template.md@{}",
                    &base[..7]
                )),
                "{preview}"
            );
            assert!(
                preview.contains("Title: Ordinary template prose"),
                "{preview}"
            );
        } else {
            assert!(preview.contains("body source: knit"), "{preview}");
            assert!(!preview.contains("Ordinary template prose"), "{preview}");
        }
        assert!(!preview.contains("Feature template"), "{preview}");
        assert!(preview.contains("<!-- BEGIN KNIT BUNDLE -->"), "{preview}");
        assert_eq!(fs::read(&artifact).unwrap(), before);
    }
    fs::remove_dir_all(root).unwrap();
}
