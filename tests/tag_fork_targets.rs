mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;

#[test]
fn tag_pins_and_exports_target_base_not_fork_origin_and_resumes_there() {
    let root = unique_temp_dir();
    let (target, checkout, collaborator) = init_remote_repo(&root, "service");
    let source = root.join("fork.git");
    git(
        &root,
        [
            "clone",
            "--bare",
            target.to_str().unwrap(),
            source.to_str().unwrap(),
        ],
    );
    git(
        &checkout,
        ["remote", "set-url", "origin", source.to_str().unwrap()],
    );
    append_line(&collaborator.join("app.txt"), "target advanced");
    git(&collaborator, ["add", "."]);
    git(&collaborator, ["commit", "-m", "Target advance"]);
    git(&collaborator, ["push", "origin", "main"]);
    let expected = git(&target, ["rev-parse", "main"]).trim().to_owned();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "synthetic tags"]);
    knit(&workspace, ["bundle", "add", checkout.to_str().unwrap()]);
    let artifact = workspace.join(".knit/bundles/synthetic-tags.bundle.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&artifact).unwrap()).unwrap();
    bundle["repos"][0]["sourceRemote"] = json!("https://github.com/contributor/service.git");
    bundle["repos"][0]["targetRemote"] = json!("https://github.com/upstream/service.git");
    for (url, path) in [
        ("https://github.com/contributor/service.git", &source),
        ("https://github.com/upstream/service.git", &target),
    ] {
        git(
            &checkout,
            ["config", &format!("url.{}.insteadOf", path.display()), url],
        );
    }
    fs::write(&artifact, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
    // Evidence collection is best effort; use an unavailable synthetic API endpoint.
    let env = [
        ("GH_TOKEN", "synthetic"),
        ("KNIT_GITHUB_API_TRANSPORT", "native"),
        ("KNIT_GITHUB_API_BASE", "http://127.0.0.1:1"),
    ];
    knit_with_env(&workspace, ["tag", "verified"], &env);
    assert_eq!(
        git(&target, ["rev-parse", "refs/tags/knit/verified^{commit}"]).trim(),
        expected
    );
    assert!(!git_success(
        &source,
        ["show-ref", "--verify", "refs/tags/knit/verified"]
    ));
    git(&target, ["tag", "-d", "knit/verified"]);
    knit_with_env(&workspace, ["tag", "verified"], &env);
    assert_eq!(
        git(&target, ["rev-parse", "refs/tags/knit/verified^{commit}"]).trim(),
        expected
    );
    assert!(!git_success(
        &source,
        ["show-ref", "--verify", "refs/tags/knit/verified"]
    ));
    // Explicit same-repository fields remain ordinary tagging, including a
    // source checkout whose origin is a different repository.
    let mut bundle: Value = serde_json::from_slice(&fs::read(&artifact).unwrap()).unwrap();
    bundle["repos"][0]["sourceRemote"] = bundle["repos"][0]["targetRemote"].clone();
    fs::write(&artifact, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
    knit_with_env(&workspace, ["tag", "same-repository"], &env);
    assert_eq!(
        git(
            &target,
            ["rev-parse", "refs/tags/knit/same-repository^{commit}"]
        )
        .trim(),
        expected
    );
    assert!(!git_success(
        &source,
        ["show-ref", "--verify", "refs/tags/knit/same-repository"]
    ));
}
