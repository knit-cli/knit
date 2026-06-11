mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// Add a `runtime` block to the workspace project artifact, pointing at a
/// stack repo compose file with a per-bundle database.
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

fn setup_runtime_workspace(root: &Path) -> std::path::PathBuf {
    let stack = root.join("stack");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&stack, "stack");

    knit(&workspace, ["init", "arbient"]);
    knit(&workspace, ["project", "add", "stack", stack.to_str().unwrap()]);
    knit(&workspace, ["bundle", "venue capacity"]);
    write_project_runtime(&workspace, "arbient");
    workspace
}

#[test]
fn run_up_requires_stack_repo_compose_file() {
    let root = unique_temp_dir();
    let workspace = setup_runtime_workspace(&root);

    let output = knit_fails(&workspace, ["run", "up"]);
    assert!(
        output.contains("Runtime compose file not found"),
        "unexpected output: {output}"
    );
    assert!(output.contains("docker-compose.knit.yml"));
    assert!(output.contains("KNIT_CHECKOUT_<repo>"));

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn run_up_injects_environment_contract_into_repo_compose_file() {
    let root = unique_temp_dir();
    let workspace = setup_runtime_workspace(&root);

    let stack_checkout = workspace.join(".knit/worktrees/venue-capacity/stack");
    fs::write(
        stack_checkout.join("docker-compose.knit.yml"),
        "services:\n  backend:\n    image: scratch\n",
    )
    .unwrap();

    let fake_bin = root.join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let log_dir = root.join("fake-docker-logs");
    fs::create_dir_all(&log_dir).unwrap();
    let docker = fake_bin.join("docker");
    fs::write(
        &docker,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FAKE_DOCKER_DIR/calls.log\"\nenv | grep -E '^(KNIT_|COMPOSE_PROJECT_NAME)' >> \"$FAKE_DOCKER_DIR/env.log\"\nexit 0\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&docker).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&docker, permissions).unwrap();
    }

    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let output = knit_with_env(
        &workspace,
        ["run", "up"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    assert!(output.contains("Runtime up:"), "unexpected output: {output}");

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
    assert!(env.contains("KNIT_DB_NAME=knithub_venue-capacity"));
    assert!(env.contains("KNIT_DB_HOST=db"));
    assert!(env.contains("KNIT_DB_HOST_PORT=5437"));

    // Run state records the injected contract for manual reproduction.
    let state: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/runtime-runs/venue-capacity/state.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(state["databaseMode"], "bundle");
    assert_eq!(state["profiles"], json!(["bundle-db"]));
    assert_eq!(state["env"]["KNIT_BUNDLE"], "venue-capacity");

    fs::remove_dir_all(root).unwrap();
}
