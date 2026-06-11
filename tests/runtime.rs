mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// Add a `runtime` block to the workspace project artifact, pointing at a
/// stack repo contract compose file with a per-bundle database.
fn write_project_runtime(workspace: &Path, project_id: &str) {
    let path = workspace
        .join(".knit/projects")
        .join(format!("{project_id}.project.json"));
    let mut project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    project["runtime"] = json!({
        "kind": "docker-compose",
        "stackRepo": "stack",
        "composeFile": "docker-compose.knit.yml",
        "database": { "mode": "bundle" },
        "ports": { "backendBase": 4901, "frontendBase": 5901, "step": 7 }
    });
    fs::write(&path, serde_json::to_string_pretty(&project).unwrap()).unwrap();
}

fn setup_workspace(root: &Path, with_runtime_block: bool) -> std::path::PathBuf {
    let stack = root.join("stack");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&stack, "stack");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "stack", stack.to_str().unwrap()],
    );
    knit(&workspace, ["bundle", "venue capacity"]);
    if with_runtime_block {
        write_project_runtime(&workspace, "demo");
    }
    workspace
}

#[cfg(unix)]
fn write_fake_docker(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let fake_bin = root.join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let log_dir = root.join("fake-docker-logs");
    fs::create_dir_all(&log_dir).unwrap();
    let docker = fake_bin.join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
case " $* " in
  *" config "*) cat "$FAKE_DOCKER_DIR/config.json"; exit 0;;
  *" ls "*) exit 0;;
esac
printf '%s\n' "$*" >> "$FAKE_DOCKER_DIR/calls.log"
env | grep -E '^(KNIT_|COMPOSE_PROJECT_NAME)' >> "$FAKE_DOCKER_DIR/env.log" 2>/dev/null
exit 0
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(&docker).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&docker, permissions).unwrap();
    (fake_bin, log_dir)
}

#[test]
fn bare_knit_run_does_not_start_the_runtime() {
    let root = unique_temp_dir();
    let workspace = setup_workspace(&root, true);

    let output = knit_fails(&workspace, ["run"]);
    assert!(
        output.contains("Pass a project command name"),
        "bare `knit run` should ask for a command, got: {output}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn project_command_named_up_shadows_the_runtime_verb() {
    let root = unique_temp_dir();
    let workspace = setup_workspace(&root, true);

    let stack_checkout = workspace.join(".knit/worktrees/venue-capacity/stack");
    fs::write(
        stack_checkout.join("docker-compose.knit.yml"),
        "services:\n  backend:\n    image: scratch\n    ports:\n      - \"${KNIT_PORT_BACKEND}:4000\"\n",
    )
    .unwrap();
    knit(
        &workspace,
        [
            "project",
            "command",
            "set",
            "up",
            "--repo",
            "stack",
            "--",
            "echo",
            "project-up-ran",
        ],
    );

    let (fake_bin, log_dir) = write_fake_docker(&root);
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let output = knit_with_env(
        &workspace,
        ["run", "up"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    assert!(
        output.contains("project-up-ran"),
        "configured project command should win, got: {output}"
    );
    assert!(
        !log_dir.join("calls.log").exists(),
        "runtime docker must not run when a project command shadows `up`"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn run_up_requires_a_compose_file() {
    let root = unique_temp_dir();
    let workspace = setup_workspace(&root, true);

    let output = knit_fails(&workspace, ["run", "up"]);
    assert!(
        output.contains("Runtime compose file not found"),
        "unexpected output: {output}"
    );
    assert!(output.contains("docker-compose.knit.yml"));

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn run_up_contract_mode_injects_environment_into_repo_compose_file() {
    let root = unique_temp_dir();
    let workspace = setup_workspace(&root, true);

    let stack_checkout = workspace.join(".knit/worktrees/venue-capacity/stack");
    // References KNIT_* variables, so it opts into contract mode.
    fs::write(
        stack_checkout.join("docker-compose.knit.yml"),
        "services:\n  backend:\n    image: scratch\n    ports:\n      - \"${KNIT_PORT_BACKEND}:4000\"\n",
    )
    .unwrap();

    let (fake_bin, log_dir) = write_fake_docker(&root);
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let output = knit_with_env(
        &workspace,
        ["run", "up"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    assert!(
        output.contains("Runtime up:"),
        "unexpected output: {output}"
    );

    let calls = fs::read_to_string(log_dir.join("calls.log")).unwrap();
    assert!(calls.contains("-p knit-run-venue-capacity"));
    assert!(calls.contains("--profile bundle-db"));
    assert!(calls.contains("up --build -d"));
    assert!(calls.contains("docker-compose.knit.yml"));

    let env = fs::read_to_string(log_dir.join("env.log")).unwrap();
    assert!(env.contains("KNIT_BUNDLE=venue-capacity"));
    assert!(env.contains("COMPOSE_PROJECT_NAME=knit-run-venue-capacity"));
    assert!(env.contains("KNIT_CHECKOUT_STACK="));
    assert!(env.contains("KNIT_SRC_STACK=.knit/worktrees/venue-capacity/stack"));
    assert!(env.contains("KNIT_REV_STACK="));
    assert!(env.contains("KNIT_PORT_BACKEND="));
    assert!(env.contains("KNIT_PORT_FRONTEND="));
    assert!(env.contains("KNIT_DB_MODE=bundle"));
    assert!(env.contains("KNIT_DB_NAME=app_venue-capacity"));
    assert!(env.contains("KNIT_DB_HOST=db"));
    assert!(env.contains("KNIT_DB_HOST_PORT=5437"));

    // Run state records the injected contract for manual reproduction.
    let state: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/runtime-runs/venue-capacity/state.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(state["mode"], "contract");
    assert_eq!(state["database"]["mode"], "bundle");
    assert_eq!(state["profiles"], json!(["bundle-db"]));
    assert_eq!(state["env"]["KNIT_BUNDLE"], "venue-capacity");

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn run_up_transform_mode_lifts_main_shape_with_zero_config() {
    let root = unique_temp_dir();
    // No runtime block at all: the single bundle repo with a compose file is
    // detected automatically.
    let workspace = setup_workspace(&root, false);

    let stack_checkout = workspace.join(".knit/worktrees/venue-capacity/stack");
    fs::write(
        stack_checkout.join("docker-compose.yml"),
        "services:\n  app:\n    build: .\n    ports:\n      - \"47300:8080\"\n",
    )
    .unwrap();

    let (fake_bin, log_dir) = write_fake_docker(&root);

    // Canned `docker compose config` output, resolved in source-space the way
    // real compose would resolve it from the source repo directory.
    let stack_source = root.join("stack");
    fs::write(
        log_dir.join("config.json"),
        serde_json::to_string_pretty(&json!({
            "name": "stack",
            "services": {
                "app": {
                    "container_name": "stack-app",
                    "build": { "context": stack_source.display().to_string() },
                    "environment": { "SELF_URL": "http://localhost:47300" },
                    "ports": [
                        {"mode": "ingress", "target": 8080, "published": "47300", "protocol": "tcp"}
                    ]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let output = knit_with_env(
        &workspace,
        ["run", "up"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    assert!(
        output.contains("Runtime up:"),
        "unexpected output: {output}"
    );

    let calls = fs::read_to_string(log_dir.join("calls.log")).unwrap();
    assert!(calls.contains("-p knit-run-venue-capacity"));
    assert!(calls.contains("up --build -d"));

    // The generated compose file has the bundle worktree substituted for the
    // source checkout, a fresh host port, and rewritten port references.
    let generated_path = workspace.join(".knit/runtime-runs/venue-capacity/docker-compose.yml");
    let generated: Value =
        serde_json::from_str(&fs::read_to_string(&generated_path).unwrap()).unwrap();
    let app = &generated["services"]["app"];
    assert!(app.get("container_name").is_none());
    let context = app["build"]["context"].as_str().unwrap();
    assert!(
        context.ends_with(".knit/worktrees/venue-capacity/stack"),
        "context not remapped to the bundle worktree: {context}"
    );
    let new_port = app["ports"][0]["published"].as_str().unwrap().to_string();
    assert_ne!(new_port, "47300");
    assert_eq!(app["ports"][0]["target"], 8080);
    assert_eq!(
        app["environment"]["SELF_URL"],
        format!("http://localhost:{new_port}")
    );

    let state: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/runtime-runs/venue-capacity/state.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(state["mode"], "transform");
    assert_eq!(state["ports"][0]["service"], "app");
    assert_eq!(state["ports"][0]["host"].to_string(), new_port);
    assert_eq!(state["ports"][0]["container"], 8080);

    fs::remove_dir_all(root).unwrap();
}
