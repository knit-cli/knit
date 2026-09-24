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

/// The inverse of [`assert_valid`]: the published schema must reject the
/// instance, with at least one error pointing at it.
#[track_caller]
fn assert_invalid(schema: &Value, instance: &Value, label: &str) {
    let validator = jsonschema::validator_for(schema)
        .unwrap_or_else(|error| panic!("{label}: schema itself is invalid: {error}"));
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|error| format!("  at {}: {error}", error.instance_path))
        .collect();
    assert!(
        !errors.is_empty(),
        "{label} unexpectedly matches the schema Knit publishes for it"
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
        "startupTimeoutSeconds": 90,
        "database": {
            "mode": "bundle",
            "service": "postgres",
            "containerPort": 5432,
            "host": "localhost",
            "portBase": 5436,
            "repos": ["backend"]
        },
        "bindings": [
            {
                "repo": "frontend",
                "service": "web",
                "environment": "APP_API_URL",
                "target": {"repo": "backend", "service": "api", "port": 4000}
            },
            {
                "repo": "frontend",
                "service": "web",
                "buildArg": "API_ORIGIN",
                "target": {"repo": "backend", "service": "api"}
            }
        ]
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

    // Explicitly retain legacy provider fixtures; v0.2 generated plans/runs are
    // schema-checked by landing_v2 and the executor tests.
    // An intermediate lane plan: merge_branch steps, targetBranches, laneAbsent.
    knit_with_fake_gh(
        &workspace,
        ["land", "--schema-version", "0.1", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let lane_plan = read(&plan_path);
    assert_eq!(lane_plan["lane"].as_str(), Some("staging"));
    assert_valid(&plan_schema, &lane_plan, "an intermediate lane plan");

    // A terminal plan over the recorded review bases.
    knit_with_fake_gh(
        &workspace,
        ["land", "--schema-version", "0.1", "plan", "--force"],
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

/// A minimal complete project object carrying the given runtime block: the
/// published schema requires the project identity fields, so fragments
/// like `{"runtime": ...}` alone cannot validate.
fn project_with_runtime(runtime: Value) -> Value {
    json!({
        "schemaVersion": "0.1",
        "kind": "KnitProject",
        "id": "demo",
        "createdAt": "2026-09-21T00:00:00Z",
        "updatedAt": "2026-09-21T00:00:00Z",
        "repos": [],
        "runtime": runtime
    })
}

/// The runtime's endpoint bindings, database repo list, and startup timeout
/// must keep schema types and serde round-trips in lockstep: camelCase wire
/// names, defaults omitted, and both authored and re-serialized shapes
/// valid against the published schema.
#[test]
fn runtime_bindings_database_repos_and_startup_timeout_roundtrip() {
    let schema: Value = serde_json::from_str(
        &fs::read_to_string("schemas/project.schema.json").expect("run from the repository root"),
    )
    .unwrap();

    let runtime_json = json!({
        "kind": "docker-compose",
        "startupTimeoutSeconds": 90,
        "bindings": [
            {
                "repo": "frontend",
                "service": "web",
                "environment": "APP_API_URL",
                "target": {"repo": "backend", "service": "api", "port": 4000}
            },
            {
                "repo": "frontend",
                "service": "web",
                "buildArg": "API_ORIGIN",
                "target": {"repo": "backend", "service": "api"}
            }
        ],
        "database": {
            "mode": "bundle",
            "service": "postgres",
            "repos": ["backend"]
        }
    });

    let runtime: knit::model::ProjectRuntime = serde_json::from_value(runtime_json.clone())
        .expect("the authored runtime block deserializes");
    assert_eq!(runtime.startup_timeout_seconds, 90);
    assert_eq!(runtime.bindings.len(), 2);
    assert_eq!(
        runtime.bindings[0].environment.as_deref(),
        Some("APP_API_URL")
    );
    assert!(runtime.bindings[0].build_arg.is_none());
    assert_eq!(runtime.bindings[0].target.port, Some(4000));
    assert_eq!(runtime.bindings[1].build_arg.as_deref(), Some("API_ORIGIN"));
    assert!(runtime.bindings[1].environment.is_none());
    assert_eq!(runtime.bindings[1].target.port, None);
    assert_eq!(
        runtime.database.as_ref().unwrap().repos,
        vec!["backend".to_string()]
    );

    let written = serde_json::to_value(&runtime).unwrap();
    assert_eq!(written["startupTimeoutSeconds"], json!(90));
    assert_eq!(written["bindings"][1]["buildArg"], json!("API_ORIGIN"));
    assert!(written["bindings"][0].get("buildArg").is_none());
    assert!(written["bindings"][0].get("environment").is_some());
    assert_eq!(written["database"]["repos"], json!(["backend"]));

    assert_valid(
        &schema,
        &project_with_runtime(runtime_json),
        "an authored runtime bindings block",
    );
    assert_valid(
        &schema,
        &project_with_runtime(written.clone()),
        "a round-tripped runtime bindings block",
    );
    let again: knit::model::ProjectRuntime = serde_json::from_value(written).unwrap();
    assert_eq!(
        again.startup_timeout_seconds,
        runtime.startup_timeout_seconds
    );
    assert_eq!(again.bindings.len(), runtime.bindings.len());
    assert_eq!(
        again.database.unwrap().repos,
        runtime.database.unwrap().repos
    );

    // Defaults: absent fields deserialize (120s timeout, no bindings, no
    // database repos) and are omitted on write, so existing projects stay
    // byte-compatible.
    let bare: knit::model::ProjectRuntime = serde_json::from_value(json!({})).unwrap();
    assert_eq!(bare.startup_timeout_seconds, 120);
    assert!(bare.bindings.is_empty());
    assert!(knit::model::ProjectRuntimeDatabase::default()
        .repos
        .is_empty());
    let default_written = serde_json::to_value(&bare).unwrap();
    assert!(default_written.get("startupTimeoutSeconds").is_none());
    assert!(default_written.get("bindings").is_none());
    assert_valid(
        &schema,
        &project_with_runtime(default_written),
        "a default runtime block",
    );
}

/// A binding must select exactly one of `environment`/`buildArg` (the
/// schema's `oneOf`) and never disambiguate with container port 0; the
/// published schema rejects every other shape so typoed projects fail at
/// the door instead of mid-run.
#[test]
fn runtime_bindings_schema_rejects_broken_selections() {
    let schema: Value = serde_json::from_str(
        &fs::read_to_string("schemas/project.schema.json").expect("run from the repository root"),
    )
    .unwrap();
    let runtime_with = |binding: Value| project_with_runtime(json!({"bindings": [binding]}));

    // Exactly one selector: valid, and stays valid when the target
    // carries a container-port disambiguator.
    for valid in [
        json!({
            "repo": "frontend", "service": "web", "environment": "APP_API_URL",
            "target": {"repo": "backend", "service": "api", "port": 4000}
        }),
        json!({
            "repo": "frontend", "service": "web", "buildArg": "API_ORIGIN",
            "target": {"repo": "backend", "service": "api"}
        }),
    ] {
        assert_valid(&schema, &runtime_with(valid), "a well-formed binding");
    }

    // Neither selector: rejected.
    assert_invalid(
        &schema,
        &runtime_with(json!({
            "repo": "frontend", "service": "web",
            "target": {"repo": "backend", "service": "api"}
        })),
        "a binding selecting neither environment nor buildArg",
    );

    // Both selectors: rejected.
    assert_invalid(
        &schema,
        &runtime_with(json!({
            "repo": "frontend", "service": "web",
            "environment": "APP_API_URL", "buildArg": "API_ORIGIN",
            "target": {"repo": "backend", "service": "api"}
        })),
        "a binding selecting both environment and buildArg",
    );

    // Zero container port: rejected (ports start at 1).
    assert_invalid(
        &schema,
        &runtime_with(json!({
            "repo": "frontend", "service": "web", "environment": "APP_API_URL",
            "target": {"repo": "backend", "service": "api", "port": 0}
        })),
        "a binding with target.port 0",
    );
}
