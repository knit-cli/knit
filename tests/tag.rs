mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Two repos with bare origins, one bundle with a committed change. Returns
/// (workspace, backend, frontend, backend_remote, backend_collaborator).
fn setup_remote_bundle(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (backend_remote, backend, backend_collaborator) = init_remote_repo(root, "backend");
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
        "backend change",
    );
    knit(&workspace, ["commit", "--all", "-m", "Feature change"]);

    (
        workspace,
        backend,
        frontend,
        backend_remote,
        backend_collaborator,
    )
}

fn tag_nodes(bundle: &Value, name: &str) -> Vec<Value> {
    bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| {
            node["type"].as_str() == Some("tag.created") && node["title"].as_str() == Some(name)
        })
        .cloned()
        .collect()
}

#[test]
fn tag_creates_annotated_tags_pins_origin_base_and_records_node() {
    let root = unique_temp_dir();
    let (workspace, backend, frontend, backend_remote, _collab) = setup_remote_bundle(&root);

    let output = knit(&workspace, ["tag", "v1-launch"]);
    assert!(output.contains("Tag:"), "{output}");
    assert!(output.contains("tagged"), "{output}");
    assert!(output.contains("pushed"), "{output}");

    // Annotated tag objects in both source checkouts.
    for repo in [&backend, &frontend] {
        let object_type = git(
            repo,
            [
                "for-each-ref",
                "refs/tags/knit/v1-launch",
                "--format=%(objecttype)",
            ],
        );
        assert_eq!(object_type.trim(), "tag");
    }

    // The bare origin has the tag, peeling to the freshly fetched origin base.
    let remote_pin = git(
        &backend_remote,
        ["rev-parse", "refs/tags/knit/v1-launch^{commit}"],
    );
    let origin_main = git(&backend, ["rev-parse", "origin/main"]);
    assert_eq!(remote_pin.trim(), origin_main.trim());

    let bundle = read_bundle(&workspace);
    let nodes = tag_nodes(&bundle, "v1-launch");
    assert_eq!(nodes.len(), 1);
    let node = &nodes[0];
    let pins = node["commits"].as_array().unwrap();
    assert_eq!(pins.len(), 2);
    let backend_pin = pins
        .iter()
        .find(|pin| pin["repoId"].as_str() == Some("backend"))
        .unwrap();
    assert_eq!(backend_pin["sha"].as_str().unwrap(), origin_main.trim());

    let message = node["message"].as_str().unwrap();
    assert!(message.contains("bundle: venue-capacity"), "{message}");
    assert!(message.contains("configured-base CI"), "{message}");
    // Local bare origins have no GitHub slug, so CI evidence degrades to
    // unknown without blocking the tag.
    assert!(message.contains("unknown (no GitHub remote)"), "{message}");
    assert!(message.contains("pins:"), "{message}");

    assert!(knit(&workspace, ["bundle", "validate"]).contains("Bundle valid"));
    assert!(knit(&workspace, ["log"]).contains("tag knit/v1-launch"));
}

#[test]
fn tag_fetches_so_pin_tracks_advanced_origin() {
    let root = unique_temp_dir();
    let (workspace, _backend, _frontend, _backend_remote, collaborator) =
        setup_remote_bundle(&root);

    append_line(&collaborator.join("app.txt"), "collaborator change");
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Collaborator change"]);
    git(&collaborator, ["push", "origin", "main"]);
    let advanced = git(&collaborator, ["rev-parse", "HEAD"]);

    // No local pull: the tag must fetch and pin the advanced origin base.
    knit(&workspace, ["tag", "v2"]);
    let bundle = read_bundle(&workspace);
    let node = &tag_nodes(&bundle, "v2")[0];
    let backend_pin = node["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pin| pin["repoId"].as_str() == Some("backend"))
        .unwrap();
    assert_eq!(backend_pin["sha"].as_str().unwrap(), advanced.trim());
}

#[test]
fn tag_subset_marks_partial_and_list_shows_it() {
    let root = unique_temp_dir();
    let (workspace, _backend, _frontend, _backend_remote, _collab) = setup_remote_bundle(&root);

    let full = knit(&workspace, ["tag", "v-full"]);
    assert!(!full.contains("partial set weakens"), "{full}");

    let subset = knit(&workspace, ["tag", "hot-1", "-r", "backend"]);
    assert!(
        subset.contains("partial set weakens the known-good claim"),
        "{subset}"
    );
    let bundle = read_bundle(&workspace);
    let node = &tag_nodes(&bundle, "hot-1")[0];
    assert_eq!(node["commits"].as_array().unwrap().len(), 1);
    assert!(node["message"]
        .as_str()
        .unwrap()
        .contains("partial — not tagged: frontend"));

    let list = knit(&workspace, ["tag"]);
    assert!(list.contains("knit/v-full"), "{list}");
    assert!(list.contains("2/2"), "{list}");
    assert!(list.contains("knit/hot-1"), "{list}");
    assert!(list.contains("1/2"), "{list}");
    assert!(list.contains("partial (missing: frontend)"), "{list}");

    let list_subcommand = knit(&workspace, ["tag", "list"]);
    assert!(list_subcommand.contains("knit/hot-1"), "{list_subcommand}");
}

#[test]
fn tag_show_reports_shas_subject_and_provenance() {
    let root = unique_temp_dir();
    let (workspace, backend, _frontend, _backend_remote, _collab) = setup_remote_bundle(&root);

    knit(&workspace, ["tag", "rel-s"]);
    let show = knit(&workspace, ["tag", "show", "rel-s"]);
    assert!(show.contains("knit/rel-s"), "{show}");
    assert!(show.contains("backend"), "{show}");
    assert!(show.contains("frontend"), "{show}");
    let origin_main = git(&backend, ["rev-parse", "--short=7", "origin/main"]);
    assert!(show.contains(origin_main.trim()), "{show}");
    assert!(show.contains("Subject:"), "{show}");
    assert!(show.contains("known-good main"), "{show}");
    assert!(show.contains("Bundle:"), "{show}");
    assert!(show.contains("venue-capacity"), "{show}");
    assert!(show.contains("Node:"), "{show}");

    let missing = knit_fails(&workspace, ["tag", "show", "nope"]);
    assert!(missing.contains("No tag `knit/nope` found"), "{missing}");
}

#[test]
fn tag_refuses_existing_tags_and_invalid_names() {
    let root = unique_temp_dir();
    let (workspace, _backend, frontend, _backend_remote, collaborator) = setup_remote_bundle(&root);

    // A colliding tag pushed by someone else: refused, and nothing is created
    // anywhere (a same-bundle rerun would be the resume path instead).
    git(&collaborator, ["tag", "knit/v6"]);
    git(&collaborator, ["push", "origin", "refs/tags/knit/v6"]);
    let refused = knit_fails(&workspace, ["tag", "v6"]);
    assert!(refused.contains("already exists"), "{refused}");
    assert!(refused.contains("backend (origin)"), "{refused}");
    assert_eq!(git(&frontend, ["tag", "--list", "knit/v6"]).trim(), "");
    let bundle = read_bundle(&workspace);
    assert!(tag_nodes(&bundle, "v6").is_empty());

    for bad in ["v1..2", ".hidden", "v1.lock"] {
        let output = knit_fails(&workspace, ["tag", bad]);
        assert!(output.contains("is invalid"), "{bad}: {output}");
    }
}

#[test]
fn tag_no_push_then_rerun_resumes_and_pushes_once() {
    let root = unique_temp_dir();
    let (workspace, backend, _frontend, backend_remote, _collab) = setup_remote_bundle(&root);

    let first = knit(&workspace, ["tag", "v3", "--no-push"]);
    assert!(first.contains("tagged"), "{first}");
    assert!(!git(&backend, ["tag", "--list", "knit/v3"])
        .trim()
        .is_empty());
    assert_eq!(
        git(&backend_remote, ["tag", "--list", "knit/v3"]).trim(),
        ""
    );

    let rerun = knit(&workspace, ["tag", "v3"]);
    assert!(rerun.contains("pushed"), "{rerun}");
    assert!(!git(&backend_remote, ["tag", "--list", "knit/v3"])
        .trim()
        .is_empty());

    // Converged: still exactly one ledger node for the tag.
    let bundle = read_bundle(&workspace);
    assert_eq!(tag_nodes(&bundle, "v3").len(), 1);

    // A further rerun is a no-op.
    let idempotent = knit(&workspace, ["tag", "v3"]);
    assert!(idempotent.contains("up to date"), "{idempotent}");
}

#[test]
fn tag_resume_recreates_deleted_local_tag_and_rejects_moved_one() {
    let root = unique_temp_dir();
    let (workspace, backend, _frontend, _backend_remote, _collab) = setup_remote_bundle(&root);

    knit(&workspace, ["tag", "v4", "--no-push"]);
    let pinned = git(&backend, ["rev-parse", "refs/tags/knit/v4^{commit}"]);

    // Deleted local tag: resume recreates it at the ledger pin.
    git(&backend, ["tag", "-d", "knit/v4"]);
    let resumed = knit(&workspace, ["tag", "v4", "--no-push"]);
    assert!(resumed.contains("recreated"), "{resumed}");
    assert_eq!(
        git(&backend, ["rev-parse", "refs/tags/knit/v4^{commit}"]).trim(),
        pinned.trim()
    );

    // Moved local tag: immutable, resume refuses naming the repo.
    git(&backend, ["tag", "-d", "knit/v4"]);
    append_line(&backend.join("app.txt"), "moved");
    git(&backend, ["add", "app.txt"]);
    git(&backend, ["commit", "-m", "Move main"]);
    git(&backend, ["tag", "knit/v4"]);
    let refused = knit_fails(&workspace, ["tag", "v4"]);
    assert!(refused.contains("backend"), "{refused}");
    assert!(refused.contains("pinned"), "{refused}");
}

#[test]
fn tag_no_git_records_node_only() {
    let root = unique_temp_dir();
    let (workspace, backend, frontend, _backend_remote, _collab) = setup_remote_bundle(&root);

    let output = knit(&workspace, ["tag", "v5", "--no-git"]);
    assert!(output.contains("recorded"), "{output}");
    assert_eq!(git(&backend, ["tag", "--list", "knit/*"]).trim(), "");
    assert_eq!(git(&frontend, ["tag", "--list", "knit/*"]).trim(), "");

    let bundle = read_bundle(&workspace);
    let node = &tag_nodes(&bundle, "v5")[0];
    assert_eq!(node["commits"].as_array().unwrap().len(), 2);
    assert!(knit(&workspace, ["bundle", "validate"]).contains("Bundle valid"));
}

#[test]
fn tag_works_on_archived_bundle_and_list_falls_back_to_project() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _backend_collab) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _frontend_collab) = init_remote_repo(&root, "frontend");
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
    knit(&workspace, ["bundle", "venue capacity"]);
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend change",
    );
    knit(&workspace, ["commit", "--all", "-m", "Feature change"]);

    knit(&workspace, ["tag", "v1"]);

    // The tag lands in the project history ledger as its own event kind, with
    // the tag name carried in metadata so consumers need not parse the message.
    let history = fs::read_to_string(workspace.join(".knit/history/demo.history.jsonl")).unwrap();
    let tag_event = history
        .lines()
        .find(|line| line.contains("commit.tagged"))
        .expect("tag history event");
    assert!(tag_event.contains("tag.created"), "{tag_event}");
    assert!(tag_event.contains("\"title\":\"v1\""), "{tag_event}");

    knit(&workspace, ["bundle", "archive", "venue-capacity"]);

    // No active bundle after archiving: the read-only verbs fall back to the
    // project's repos.
    let list = knit(&workspace, ["tag"]);
    assert!(list.contains("knit/v1"), "{list}");
    assert!(list.contains("2/2"), "{list}");

    // Creating on the archived bundle works via explicit --bundle.
    let output = knit(&workspace, ["--bundle", "venue-capacity", "tag", "v-post"]);
    assert!(output.contains("tagged"), "{output}");
    let bundle = read_bundle(&workspace);
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    assert_eq!(tag_nodes(&bundle, "v-post").len(), 1);
}

#[test]
fn tag_records_configured_base_ci_evidence() {
    let root = unique_temp_dir();
    let (workspace, backend, _frontend, _backend_remote, _collab) = setup_remote_bundle(&root);

    // GitHub-shaped remotes so the provider derives acme/<repo> slugs; the
    // native transport then talks to the fake API server.
    let artifact = workspace.join(".knit/bundles/venue-capacity.bundle.json");
    let mut payload: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    for repo in payload["repos"].as_array_mut().unwrap() {
        let id = repo["id"].as_str().unwrap().to_string();
        repo["remote"] = json!(format!("https://github.com/acme/{id}.git"));
    }
    fs::write(&artifact, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

    let fake_gh_dir = root.join("fake-gh");
    fs::create_dir_all(&fake_gh_dir).unwrap();
    fs::write(fake_gh_dir.join("ci-pass-backend"), "").unwrap();
    fs::write(fake_gh_dir.join("ci-fail-frontend"), "").unwrap();
    let api_base = spawn_fake_github_api(&fake_gh_dir);

    let output = knit_with_env(
        &workspace,
        ["tag", "rel-ci"],
        &[
            ("KNIT_GITHUB_API_TRANSPORT", "native"),
            ("KNIT_GITHUB_API_BASE", api_base.as_str()),
            ("GH_TOKEN", "gho_fake_token"),
        ],
    );
    // Red CI warns but never blocks.
    assert!(
        output.contains("frontend: configured-base CI is failed"),
        "{output}"
    );
    assert!(output.contains("tagged"), "{output}");
    assert!(!git(&backend, ["tag", "--list", "knit/rel-ci"])
        .trim()
        .is_empty());

    let bundle = read_bundle(&workspace);
    let message = tag_nodes(&bundle, "rel-ci")[0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("backend: passed"), "{message}");
    assert!(message.contains("frontend: failed"), "{message}");
}

#[cfg(unix)]
#[test]
fn land_apply_tag_flag_records_named_tag() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);

    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote", "--tag", "rel-2"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("Feature landed"), "{apply}");
    assert!(apply.contains("Tag:"), "{apply}");

    let bundle = read_bundle(&workspace);
    assert_eq!(bundle["state"].as_str(), Some("archived"));
    assert_eq!(tag_nodes(&bundle, "rel-2").len(), 1);
    let backend = root.join("backend");
    assert!(!git(&backend, ["tag", "--list", "knit/rel-2"])
        .trim()
        .is_empty());
}

#[cfg(unix)]
#[test]
fn land_apply_auto_tag_config_defaults_to_bundle_slug() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);

    knit(&workspace, ["config", "set", "auto-tag", "true"]);
    assert!(knit(&workspace, ["config", "show"]).contains("Auto tag: true"));

    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("Tag:"), "{apply}");

    let bundle = read_bundle(&workspace);
    assert_eq!(tag_nodes(&bundle, "venue-capacity").len(), 1);
}

#[cfg(unix)]
#[test]
fn alternate_target_landing_does_not_auto_tag_configured_project_bases() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);

    let bundle_path = workspace.join(".knit/bundles/venue-capacity.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    for publication in bundle["publications"].as_array_mut().unwrap() {
        publication["baseBranch"] = json!("staging");
    }
    fs::write(
        &bundle_path,
        format!("{}\n", serde_json::to_string_pretty(&bundle).unwrap()),
    )
    .unwrap();
    fs::write(fake_gh_dir.join("create-backend.base"), "staging\n").unwrap();
    fs::write(fake_gh_dir.join("create-frontend.base"), "staging\n").unwrap();

    knit(&workspace, ["config", "set", "auto-tag", "true"]);
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(apply.contains("skipped automatic tag"), "{apply}");

    let bundle = read_bundle(&workspace);
    assert!(tag_nodes(&bundle, "venue-capacity").is_empty());
}

#[cfg(unix)]
#[test]
fn land_apply_no_tag_overrides_auto_tag_config() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);

    knit(&workspace, ["config", "set", "auto-tag", "true"]);
    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote", "--no-tag"],
        &fake_bin,
        &fake_gh_dir,
    );
    // Skipped tagging still leaves the manual-tag advice.
    assert!(
        apply.contains("knit tag <name> --bundle venue-capacity"),
        "{apply}"
    );

    let bundle = read_bundle(&workspace);
    let tagged: Vec<Value> = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"].as_str() == Some("tag.created"))
        .cloned()
        .collect();
    assert!(tagged.is_empty());
}

#[test]
fn land_apply_from_artifact_rejects_tag_flags() {
    let root = unique_temp_dir();
    let output = knit_fails(
        &root,
        [
            "land",
            "apply",
            "--from-artifact",
            "bundle.json",
            "--tag",
            "v1",
        ],
    );
    assert!(output.contains("--tag/--no-tag"), "{output}");
}

#[cfg(unix)]
#[test]
fn land_apply_advises_tag_command() {
    let root = unique_temp_dir();
    let (workspace, fake_bin, fake_gh_dir) = publish_two_repo_bundle(&root);

    knit_with_fake_gh(&workspace, ["land"], &fake_bin, &fake_gh_dir);
    let apply = knit_with_fake_gh(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_gh_dir,
    );
    assert!(
        apply.contains("knit tag <name> --bundle venue-capacity"),
        "{apply}"
    );

    // Following the advice records the land run in the tag's provenance.
    let tagged = knit(&workspace, ["--bundle", "venue-capacity", "tag", "rel-1"]);
    assert!(tagged.contains("tagged"), "{tagged}");
    let bundle = read_bundle(&workspace);
    let message = tag_nodes(&bundle, "rel-1")[0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("landed: run"), "{message}");
}
