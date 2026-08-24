//! The schemas `knit schema <name>` publishes must describe what Knit actually
//! writes to disk.
//!
//! These drifted several features behind the model without anyone noticing:
//! alpha.8's own project config was invalid against alpha.8's own schema, and
//! every intermediate lane plan was rejected outright. Nothing compared the two,
//! so nothing said so. This test does the comparison, on real artifacts
//! produced by real commands rather than on hand-written fixtures.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn schema(workspace: &Path, name: &str) -> Value {
    serde_json::from_str(&knit(workspace, ["schema", "print", name])).unwrap_or_else(|error| {
        panic!("`knit schema {name}` did not emit JSON: {error}");
    })
}

#[track_caller]
fn assert_valid(schema: &Value, instance: &Value, label: &str) {
    let validator = jsonschema::validator_for(schema)
        .unwrap_or_else(|error| panic!("{label}: schema itself is invalid: {error}"));
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|error| format!("  at {}: {error}", error.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "{label} does not match the schema Knit publishes for it:\n{}",
        errors.join("\n")
    );
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn only_file(dir: &Path) -> std::path::PathBuf {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    entries.sort();
    entries
        .pop()
        .unwrap_or_else(|| panic!("no artifact in {}", dir.display()))
}

/// A project exercising every landing shape at once: merge order, top-level
/// deployments, a `whenChanged` fan-out, a branch-keyed target, and a lane with
/// an absent repository.
fn rich_landing() -> Value {
    json!({
        "provider": "github",
        "onFailure": "resume",
        "merge": { "repoOrder": ["backend", "frontend"], "method": "merge" },
        "deployments": [
            {
                "id": "deploy-backend",
                "repoId": "backend",
                "whenChanged": ["backend", "frontend"],
                "timeoutSeconds": 60,
                "command": ["sh", "-c", "true"]
            },
            { "id": "deploy-frontend", "repoId": "frontend", "mode": "push" }
        ],
        "targets": {
            "release": {
                "terminal": true,
                "deployments": [
                    { "id": "release-backend", "repoId": "backend", "mode": "push" }
                ]
            }
        },
        "lanes": {
            "staging": {
                "branches": { "backend": "staging", "frontend": null },
                "deployments": [
                    {
                        "id": "stage-backend",
                        "repoId": "backend",
                        "whenChanged": ["*"],
                        "timeoutSeconds": 60,
                        "command": ["sh", "-c", "true"]
                    }
                ]
            }
        }
    })
}

#[test]
fn published_schemas_describe_the_artifacts_knit_writes() {
    let root = unique_temp_dir();
    let (_backend_remote, backend, _c1) = init_remote_repo(&root, "backend");
    let (_frontend_remote, frontend, _c2) = init_remote_repo(&root, "frontend");
    for checkout in [&backend, &frontend] {
        git(checkout, ["branch", "staging", "main"]);
        git(checkout, ["push", "origin", "staging"]);
    }
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["init", "demo"]);
    for (id, path) in [("backend", &backend), ("frontend", &frontend)] {
        knit(&workspace, ["project", "add", id, path.to_str().unwrap()]);
    }
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value = read(&project_path);
    project["landing"] = rich_landing();
    project["runtime"] = json!({
        "kind": "contract",
        "mode": "contract",
        "stacks": ["backend"],
        "stackRepo": "backend",
        "composeFile": "docker-compose.yml",
        "database": {
            "mode": "bundle",
            "service": "postgres",
            "containerPort": 5432,
            "host": "localhost",
            "portBase": 5436
        }
    });
    fs::write(
        &project_path,
        format!("{}\n", serde_json::to_string_pretty(&project).unwrap()),
    )
    .unwrap();

    let project_schema = schema(&workspace, "project");
    assert_valid(
        &project_schema,
        &read(&project_path),
        "the project artifact",
    );

    knit(&workspace, ["bundle", "schema parity"]);
    append_line(
        &workspace.join(".knit/worktrees/schema-parity/backend/app.txt"),
        "backend change",
    );
    knit(&workspace, ["commit", "--all", "-m", "Backend change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );

    let plan_schema = schema(&workspace, "land-plan");
    let plan_path = workspace.join(".knit/land-plans/schema-parity.land.json");

    // An intermediate lane plan: merge_branch steps, targetBranches, laneAbsent.
    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let lane_plan = read(&plan_path);
    assert_eq!(lane_plan["lane"].as_str(), Some("staging"));
    assert_valid(&plan_schema, &lane_plan, "an intermediate lane plan");

    // A terminal plan over the recorded review bases.
    knit_with_fake_gh(
        &workspace,
        ["land", "plan", "--force"],
        &fake_bin,
        &fake_gh_dir,
    );
    let terminal_plan = read(&plan_path);
    assert_eq!(terminal_plan["terminal"].as_bool(), Some(true));
    assert_valid(&plan_schema, &terminal_plan, "a terminal plan");

    knit_with_fake_gh(&workspace, ["land", "apply"], &fake_bin, &fake_gh_dir);
    let run = read(&only_file(&workspace.join(".knit/land-runs")));
    assert_eq!(run["status"].as_str(), Some("succeeded"));
    assert_valid(
        &schema(&workspace, "land-run"),
        &run,
        "a completed land run",
    );

    fs::remove_dir_all(root).unwrap();
}
