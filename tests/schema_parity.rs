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

    // Check real opt-in generation, including the emitted capability markers,
    // against the same published schema used for ordinary and legacy plans.
    let mut release_project = project.clone();
    release_project["landing"]["execution"] =
        json!({"mode": "repository_sequence", "repoOrder": ["backend", "frontend"]});
    release_project["landing"]["preflight"] = json!({"mergeability": "all"});
    fs::write(
        &project_path,
        serde_json::to_vec_pretty(&release_project).unwrap(),
    )
    .unwrap();
    assert_valid(
        &project_schema,
        &release_project,
        "the opt-in project artifact",
    );
    knit_with_fake_gh(
        &workspace,
        ["land", "--lane", "staging"],
        &fake_bin,
        &fake_gh_dir,
    );
    let release_plan_path = only_file(&workspace.join(".knit/land-plans"));
    let release_plan = read(&release_plan_path);
    assert_eq!(release_plan["requiredExecutorVersion"], "0.4");
    assert_eq!(release_plan["preflight"]["mergeability"], "all");
    assert_valid(
        &plan_schema,
        &release_plan,
        "the generated repository-sequence plan",
    );
    fs::write(&project_path, serde_json::to_vec_pretty(&project).unwrap()).unwrap();
    fs::remove_file(&release_plan_path).unwrap();

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

fn schema_file(name: &str) -> Value {
    serde_json::from_str(
        &fs::read_to_string(format!("schemas/{name}.schema.json"))
            .expect("run from the repository root"),
    )
    .unwrap()
}

/// A deterministic full lowercase SHA-256-shaped pin.
fn fingerprint(byte: u8) -> String {
    let digit = char::from(b'a' + (byte % 6));
    (0..64).map(|_| digit).collect()
}

/// A minimal complete schema 0.2 plan over synthetic repositories, with no
/// executor-0.4 features: exactly what generation emits by default today.
fn base_plan() -> Value {
    json!({
        "schemaVersion": "0.2",
        "kind": "KnitLandPlan",
        "id": "synthetic-land",
        "provider": "github",
        "bundleId": "synthetic-bundle",
        "bundleFingerprint": fingerprint(1),
        "projectFingerprint": fingerprint(2),
        "onFailure": "stop",
        "requiredExecutorVersion": "0.3",
        "steps": [
            {
                "id": "merge-api",
                "type": "merge_branch",
                "repoId": "api",
                "targetBranch": "staging",
                "effect": "source"
            }
        ]
    })
}

fn with_feature_capabilities(plan: &mut Value) {
    plan["requiredCapabilities"] = json!([
        "repository-sequence",
        "mergeability-preflight",
        "integration-sources"
    ]);
}

fn with_executor(plan: &mut Value, version: Value) {
    if version.is_null() {
        plan.as_object_mut()
            .unwrap()
            .remove("requiredExecutorVersion");
    } else {
        plan["requiredExecutorVersion"] = version;
    }
}

/// Interactive and manual operations need executor 0.3 features. 0.4 is a
/// later executor that still has them, 0.2 does not — the schema's
/// conditional must accept both 0.3 and 0.4 and nothing else.
#[test]
fn interactive_and_manual_steps_accept_executor_03_and_04() {
    let schema = schema_file("land-plan");
    fn interactive(plan: &mut Value) {
        plan["steps"] = json!([
            plan["steps"][0].clone(),
            {
                "id": "attach",
                "type": "run",
                "repoId": "api",
                "command": ["./release", "observe"],
                "interactive": true,
                "effect": "read_only"
            }
        ]);
    }
    fn manual(plan: &mut Value) {
        plan["steps"] = json!([
            plan["steps"][0].clone(),
            {
                "id": "inspect",
                "type": "manual",
                "instructions": "Check the synthetic service status",
                "effect": "read_only"
            }
        ]);
    }
    for attach in [interactive, manual] {
        let mut plan = base_plan();
        attach(&mut plan);
        with_executor(&mut plan, json!("0.3"));
        assert_valid(&schema, &plan, "an interactive/manual plan on executor 0.3");
        with_executor(&mut plan, json!("0.4"));
        assert_valid(&schema, &plan, "an interactive/manual plan on executor 0.4");
        with_executor(&mut plan, json!("0.2"));
        assert_invalid(&schema, &plan, "an interactive/manual plan on executor 0.2");
        with_executor(&mut plan, Value::Null);
        assert_invalid(
            &schema,
            &plan,
            "an interactive/manual plan with no executor version",
        );
    }
}

/// The executor-0.4 plan features are opt-in but not free-form: any plan
/// carrying one of them must declare schema 0.2 and executor 0.4, because a
/// legacy executor would silently discard the key while running the plan.
#[test]
fn executor_04_plan_features_require_schema_02_and_executor_04() {
    let schema = schema_file("land-plan");
    let feature = |plan: &mut Value, key: &str| match key {
        "execution" => {
            plan["execution"] = json!({"mode": "repository_sequence", "repoOrder": ["api"]});
        }
        "preflight" => {
            plan["preflight"] = json!({"mergeability": "all"});
        }
        "integrationSources" => {
            plan["integrationSources"] =
                json!({"api": {"branch": "compatibility", "sha": fingerprint(3)}});
        }
        other => panic!("unknown feature {other}"),
    };
    for key in ["execution", "preflight", "integrationSources"] {
        let mut plan = base_plan();
        feature(&mut plan, key);
        with_feature_capabilities(&mut plan);
        with_executor(&mut plan, json!("0.4"));
        assert_valid(
            &schema,
            &plan,
            &format!("a plan with {key} on executor 0.4"),
        );

        let mut legacy = base_plan();
        legacy["schemaVersion"] = json!("0.1");
        legacy.as_object_mut().unwrap().remove("bundleFingerprint");
        legacy.as_object_mut().unwrap().remove("projectFingerprint");
        legacy["onFailure"] = json!("resume");
        assert_valid(&schema, &legacy, "the legacy baseline without new fields");
        feature(&mut legacy, key);
        assert_invalid(
            &schema,
            &legacy,
            &format!("a schema 0.1 plan carrying {key}"),
        );

        let mut stale = base_plan();
        feature(&mut stale, key);
        with_executor(&mut stale, json!("0.3"));
        assert_invalid(
            &schema,
            &stale,
            &format!("a plan with {key} on executor 0.3"),
        );

        let mut unversioned = base_plan();
        feature(&mut unversioned, key);
        with_executor(&mut unversioned, Value::Null);
        assert_invalid(
            &schema,
            &unversioned,
            &format!("a plan with {key} and no executor version"),
        );
    }
}

/// The declared repository order is load-bearing, so the schema rejects the
/// shapes that could never mean anything: repeats and empty names in both the
/// plan and the project recipes.
#[test]
fn repo_order_entries_must_be_unique_and_nonempty() {
    let plan = schema_file("land-plan");
    let project = schema_file("project");
    let with_order = |order: Value| {
        let mut plan = base_plan();
        with_feature_capabilities(&mut plan);
        with_executor(&mut plan, json!("0.4"));
        plan["execution"] = json!({"mode": "repository_sequence", "repoOrder": order});
        plan
    };
    assert_valid(
        &plan,
        &with_order(json!(["api", "web"])),
        "a plan with a well-formed repository order",
    );
    assert_invalid(
        &plan,
        &with_order(json!(["api", "api"])),
        "a plan whose repository order repeats a repository",
    );
    assert_invalid(
        &plan,
        &with_order(json!(["api", ""])),
        "a plan whose repository order contains an empty name",
    );
    assert_invalid(
        &plan,
        &with_order(json!([])),
        "a plan whose repository order is empty",
    );

    let project_with_order = |order: Value| {
        json!({
            "schemaVersion": "0.1",
            "kind": "KnitProject",
            "id": "demo",
            "createdAt": "2026-09-25T00:00:00Z",
            "updatedAt": "2026-09-25T00:00:00Z",
            "repos": [],
            "landing": {
                "execution": {"mode": "repository_sequence", "repoOrder": order}
            }
        })
    };
    assert_invalid(
        &project,
        &project_with_order(json!(["api", "api"])),
        "a project recipe whose root repository order repeats a repository",
    );
    assert_invalid(
        &project,
        &project_with_order(json!(["api", ""])),
        "a project recipe whose root repository order contains an empty name",
    );
    let lane_with_order = |order: Value| {
        json!({
            "schemaVersion": "0.1",
            "kind": "KnitProject",
            "id": "demo",
            "createdAt": "2026-09-25T00:00:00Z",
            "updatedAt": "2026-09-25T00:00:00Z",
            "repos": [],
            "landing": {
                "lanes": {
                    "staging": {
                        "branches": {"api": "staging"},
                        "execution": {"mode": "repository_sequence", "repoOrder": order}
                    }
                }
            }
        })
    };
    assert_invalid(
        &project,
        &lane_with_order(json!(["api", "api"])),
        "a lane recipe whose repository order repeats a repository",
    );
}

/// An integration source pin names one exact commit: full, lowercase, 40 or
/// 64 hex digits, with a branch, and nothing else.
#[test]
fn integration_source_pins_require_full_lowercase_shas() {
    let schema = schema_file("land-plan");
    let pinned = |pin: Value| {
        let mut plan = base_plan();
        with_feature_capabilities(&mut plan);
        with_executor(&mut plan, json!("0.4"));
        plan["integrationSources"] = json!({"api": pin});
        plan
    };
    assert_valid(
        &schema,
        &pinned(json!({"branch": "compatibility", "sha": fingerprint(3)})),
        "a 64-hex lowercase pin",
    );
    assert_invalid(
        &schema,
        &pinned(json!({"branch": "", "sha": fingerprint(3)})),
        "a pin with an empty branch",
    );
    let forty = json!({"branch": "compatibility", "sha": "a".repeat(40)});
    assert_valid(&schema, &pinned(forty), "a 40-hex lowercase pin");
    for (sha, label) in [
        (fingerprint(3).to_uppercase(), "an uppercase pin"),
        ("a".repeat(12), "an abbreviated pin"),
        ("a".repeat(63), "a truncated 63-hex pin"),
        ("a".repeat(65), "an overlong pin"),
        ("g".repeat(40), "a non-hex pin"),
        ("".to_owned(), "an empty pin"),
    ] {
        assert_invalid(
            &schema,
            &pinned(json!({"branch": "compatibility", "sha": sha})),
            label,
        );
    }
    assert_invalid(
        &schema,
        &pinned(json!({"sha": fingerprint(3)})),
        "a pin with no branch",
    );
    assert_invalid(
        &schema,
        &pinned(json!({"branch": "compatibility"})),
        "a pin with no sha",
    );
    assert_invalid(
        &schema,
        &pinned(json!({"branch": "compatibility", "sha": fingerprint(3), "remote": "origin"})),
        "a pin with an unrecognized field",
    );
}

/// Scope semantics for the opt-in recipes: an explicit `null` in a lane or
/// target explicitly disables the project root policy for that scope, and the
/// object form stays valid everywhere the key is allowed.
#[test]
fn scoped_null_execution_and_preflight_are_valid_recipes() {
    let schema = schema_file("project");
    let project_with = |landing: Value| {
        json!({
            "schemaVersion": "0.1",
            "kind": "KnitProject",
            "id": "demo",
            "createdAt": "2026-09-25T00:00:00Z",
            "updatedAt": "2026-09-25T00:00:00Z",
            "repos": [],
            "landing": landing
        })
    };
    let root_and_scopes = json!({
        "execution": {"mode": "repository_sequence", "repoOrder": ["api", "web"]},
        "preflight": {"mergeability": "all"},
        "lanes": {
            "staging": {
                "branches": {"api": "staging", "web": null},
                "execution": null,
                "preflight": null
            },
            "preview": {
                "branches": {"api": "preview", "web": "preview"},
                "execution": {"mode": "repository_sequence", "repoOrder": ["web", "api"]},
                "preflight": {"mergeability": "all"}
            }
        },
        "targets": {
            "release": {
                "terminal": true,
                "execution": null,
                "preflight": null
            }
        }
    });
    assert_valid(
        &schema,
        &project_with(root_and_scopes),
        "root policies with null-disabled and object-overriding scopes",
    );

    // A scope that simply omits the keys (inheritance by absence) and a bare
    // null at the root are both well-formed.
    assert_valid(
        &schema,
        &project_with(json!({
            "execution": null,
            "preflight": null,
            "lanes": {"staging": {"branches": {"api": "staging"}}}
        })),
        "a null root policy with an inheriting lane",
    );

    // Every other shape is refused: a bare string is neither null nor object,
    // and a policy object must be exactly the policy it claims to be.
    assert_invalid(
        &schema,
        &project_with(json!({"preflight": "all"})),
        "a string preflight policy",
    );
    assert_invalid(
        &schema,
        &project_with(json!({"execution": {"mode": "repository_sequence"}})),
        "an execution policy with no repository order",
    );
    assert_invalid(
        &schema,
        &project_with(json!({"preflight": {"mergeability": "quick"}})),
        "an unsupported mergeability policy",
    );
    assert_invalid(
        &schema,
        &project_with(json!({
            "lanes": {"staging": {"branches": {"api": "staging"}, "preflight": {"unknown": true}}}
        })),
        "an unrecognized preflight policy in a lane",
    );
}

#[test]
fn release_policies_require_their_own_capability_markers() {
    let schema = schema_file("land-plan");
    let root = unique_temp_dir();
    let plan_path = root.join("synthetic.land.json");
    let write_plan = |plan: &Value| {
        fs::write(&plan_path, serde_json::to_vec_pretty(plan).unwrap()).unwrap();
    };
    let validate_args = [
        "land",
        "validate",
        "--plan",
        plan_path.to_str().unwrap(),
        "--json",
    ];

    for (key, policy, capability) in [
        (
            "execution",
            json!({"mode": "repository_sequence", "repoOrder": ["api"]}),
            "repository-sequence",
        ),
        (
            "preflight",
            json!({"mergeability": "all"}),
            "mergeability-preflight",
        ),
        (
            "integrationSources",
            json!({"api": {"branch": "compatibility", "sha": "a".repeat(40)}}),
            "integration-sources",
        ),
    ] {
        let mut plan = base_plan();
        plan["requiredExecutorVersion"] = json!("0.4");
        plan[key] = policy;
        plan["workflow"] = json!({"sequence": [{"step": "merge-api"}]});
        for invalid in [
            None,
            Some(Value::Null),
            Some(json!([])),
            Some(json!(["unrelated"])),
            Some(json!(capability)),
        ] {
            if let Some(invalid) = invalid {
                plan["requiredCapabilities"] = invalid;
            } else {
                plan.as_object_mut().unwrap().remove("requiredCapabilities");
            }
            assert_invalid(
                &schema,
                &plan,
                &format!("{key} without its capability marker"),
            );
            write_plan(&plan);
            let error = knit_fails(&root, validate_args);
            assert!(
                error.contains("requiredCapabilities") && error.contains(capability),
                "{key}: runtime must name the missing marker: {error}"
            );
        }
        plan["requiredCapabilities"] = json!([capability]);
        assert_valid(&schema, &plan, &format!("{key} with its own capability"));
        write_plan(&plan);
        let report: Value = serde_json::from_str(&knit(&root, validate_args)).unwrap();
        assert_eq!(report["valid"], true, "{key}: {report}");
        plan[key] = Value::Null;
        assert_invalid(&schema, &plan, &format!("null {key} in a saved plan"));
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ordinary_plan_versions_remain_valid_without_release_policies() {
    let schema = schema_file("land-plan");
    for version in [Value::Null, json!("0.2"), json!("0.3"), json!("0.4")] {
        let mut plan = base_plan();
        with_executor(&mut plan, version);
        assert_valid(&schema, &plan, "an ordinary plan with no release policies");
    }
    let mut legacy = base_plan();
    legacy["schemaVersion"] = json!("0.1");
    legacy["onFailure"] = json!("resume");
    legacy["requiredExecutorVersion"] = json!("0.4");
    assert_invalid(&schema, &legacy, "executor 0.4 on a legacy plan");
}

#[test]
fn release_policy_shapes_match_across_root_lane_and_target() {
    let schema = schema_file("project");
    for scope in ["root", "lane", "target"] {
        for (key, valid, invalids) in [
            (
                "execution",
                json!({"mode": "repository_sequence", "repoOrder": ["api"]}),
                vec![
                    json!({"mode": "repository_sequence", "repoOrder": []}),
                    json!({"mode": "repository_sequence", "repoOrder": ["api", "api"]}),
                    json!({"mode": "repository_sequence", "repoOrder": [""]}),
                    json!({"mode": "parallel", "repoOrder": ["api"]}),
                    json!({"mode": "repository_sequence", "repoOrder": ["api"], "extra": true}),
                ],
            ),
            (
                "preflight",
                json!({"mergeability": "all"}),
                vec![
                    json!({}),
                    json!({"mergeability": "quick"}),
                    json!({"mergeability": "all", "extra": true}),
                    json!("all"),
                ],
            ),
        ] {
            let project_with = |policy: Value| {
                let mut config = json!({});
                config[key] = policy;
                let landing = match scope {
                    "root" => config,
                    "lane" => {
                        config["branches"] = json!({"api": "staging"});
                        json!({"lanes": {"staging": config}})
                    }
                    _ => json!({"targets": {"staging": config}}),
                };
                json!({"schemaVersion": "0.1", "kind": "KnitProject", "id": "synthetic",
                    "createdAt": "2026-09-25T00:00:00Z", "updatedAt": "2026-09-25T00:00:00Z",
                    "repos": [], "landing": landing})
            };
            assert_valid(
                &schema,
                &project_with(valid),
                &format!("{scope} {key} object"),
            );
            assert_valid(
                &schema,
                &project_with(Value::Null),
                &format!("{scope} {key} null"),
            );
            for invalid in invalids {
                assert_invalid(
                    &schema,
                    &project_with(invalid),
                    &format!("invalid {scope} {key}"),
                );
            }
        }
    }
}
