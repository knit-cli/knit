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
    write_fake_docker_state(&log_dir);
    let docker = fake_bin.join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
case " $* " in
  *" config "*) python3 "$FAKE_DOCKER_DIR/state.py" config "$@"; exit $?;;
  *" ls "*) test ! -f "$FAKE_DOCKER_DIR/projects.log" || cat "$FAKE_DOCKER_DIR/projects.log"; exit 0;;
  *" ps "*)
    printf '%s\n' "$*" >> "$FAKE_DOCKER_DIR/calls.log"
    python3 "$FAKE_DOCKER_DIR/state.py" ps "$@"; exit $?;;
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

#[cfg(unix)]
fn write_fake_docker_state(log_dir: &Path) {
    fs::write(log_dir.join("state.py"), r#"import json, os, pathlib, sys
root = pathlib.Path(os.environ["FAKE_DOCKER_DIR"])
args = sys.argv[2:]
file = pathlib.Path(args[args.index("-f") + 1]) if "-f" in args else None
if sys.argv[1] == "ps" and (root / "status.json").exists():
    print((root / "status.json").read_text())
    sys.exit(0)
config = None
if file:
    try:
        config = json.loads(file.read_text())
    except (ValueError, OSError):
        pass
if config is None:
    fixture = root / "config.json"
    for repo in ("alpha", "beta", "gamma"):
        if file and (f"/{repo}/" in str(file) or f".{repo}.yml" in str(file)):
            fixture = root / f"config-{repo}.json"
    config = json.loads(fixture.read_text()) if fixture.exists() else {"services":{"backend":{"image":"scratch"}}}
if sys.argv[1] == "config":
    print(json.dumps(config))
else:
    print(json.dumps([{"Service":name,"State":"running","Health":"healthy" if "healthcheck" in service else "","ExitCode":0}
        for name, service in config["services"].items()]))
"#).unwrap();
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
        "services:\n  backend:\n    image: scratch\n    ports:\n      - \"${KNIT_PORT_BACKEND:-4901}:4000\"\n",
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
fn run_down_purge_removes_bundle_owned_volumes_and_local_images() {
    let root = unique_temp_dir();
    let workspace = setup_workspace(&root, true);
    let stack_checkout = workspace.join(".knit/worktrees/venue-capacity/stack");
    fs::write(
        stack_checkout.join("docker-compose.knit.yml"),
        "services:\n  backend:\n    image: scratch\n    ports:\n      - \"${KNIT_PORT_BACKEND}:4000\"\n",
    )
    .unwrap();

    let (fake_bin, log_dir) = write_fake_docker(&root);
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let env = [
        ("PATH", path.as_str()),
        ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
    ];
    knit_with_env(&workspace, ["run", "up"], &env);
    fs::remove_file(log_dir.join("calls.log")).unwrap();

    let output = knit_with_env(&workspace, ["run", "down", "--purge"], &env);
    assert!(
        output.contains("Runtime purged:"),
        "unexpected output: {output}"
    );
    let calls = fs::read_to_string(log_dir.join("calls.log")).unwrap();
    assert!(calls.contains(
        "-p knit-run-venue-capacity --profile bundle-db down --remove-orphans --volumes --rmi local"
    ));
    assert!(
        !workspace.join(".knit/runtime-runs/venue-capacity").exists(),
        "purge should remove generated runtime state"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn bundle_archive_automatically_purges_runtime_resources() {
    let root = unique_temp_dir();
    // Exercise the zero-config path: cleanup must detect the compose-bearing
    // checkout before archive removes it, even with no project runtime block.
    let workspace = setup_workspace(&root, false);
    let stack_checkout = workspace.join(".knit/worktrees/venue-capacity/stack");
    fs::write(
        stack_checkout.join("docker-compose.knit.yml"),
        "services:\n  backend:\n    image: scratch\n    ports:\n      - \"${KNIT_PORT_BACKEND:-4901}:4000\"\n",
    )
    .unwrap();
    git(&stack_checkout, ["add", "docker-compose.knit.yml"]);
    git(
        &stack_checkout,
        ["commit", "-m", "Add bundle runtime compose file"],
    );

    let (fake_bin, log_dir) = write_fake_docker(&root);
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let env = [
        ("PATH", path.as_str()),
        ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
    ];
    knit_with_env(&workspace, ["run", "up"], &env);
    fs::remove_file(log_dir.join("calls.log")).unwrap();

    let output = knit_with_env(&workspace, ["bundle", "archive", "venue-capacity"], &env);
    assert!(
        output.contains("Archived bundle:"),
        "unexpected output: {output}"
    );
    let calls = fs::read_to_string(log_dir.join("calls.log")).unwrap();
    assert!(
        calls.contains("-p knit-run-venue-capacity down --remove-orphans --volumes --rmi local")
    );
    assert!(
        !workspace.join(".knit/worktrees/venue-capacity").exists(),
        "archive should remove generated worktrees"
    );

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

#[cfg(unix)]
fn write_fake_docker_multi(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let fake_bin = root.join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let log_dir = root.join("fake-docker-logs");
    fs::create_dir_all(&log_dir).unwrap();
    write_fake_docker_state(&log_dir);
    let docker = fake_bin.join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
case " $* " in
  *" config "*) python3 "$FAKE_DOCKER_DIR/state.py" config "$@"; exit $?;;
  *" ls "*) test ! -f "$FAKE_DOCKER_DIR/projects.log" || cat "$FAKE_DOCKER_DIR/projects.log"; exit 0;;
  *" ps "*)
    printf '%s\n' "$*" >> "$FAKE_DOCKER_DIR/calls.log"
    python3 "$FAKE_DOCKER_DIR/state.py" ps "$@"; exit $?;;
esac
printf '%s\n' "$*" >> "$FAKE_DOCKER_DIR/calls.log"
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

#[cfg(unix)]
#[test]
fn run_up_lifts_every_compose_repo_and_cross_wires_ports() {
    let root = unique_temp_dir();
    let alpha = root.join("alpha");
    let beta = root.join("beta");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&alpha, "alpha");
    init_repo(&beta, "beta");

    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "alpha", alpha.to_str().unwrap()],
    );
    knit(
        &workspace,
        ["project", "add", "beta", beta.to_str().unwrap()],
    );
    knit(&workspace, ["bundle", "venue capacity"]);

    for repo in ["alpha", "beta"] {
        fs::write(
            workspace.join(format!(
                ".knit/worktrees/venue-capacity/{repo}/docker-compose.yml"
            )),
            "services:\n  app:\n    build: .\n",
        )
        .unwrap();
    }

    let (fake_bin, log_dir) = write_fake_docker_multi(&root);
    // Canned `docker compose config` outputs, resolved in source-space. Each
    // stack references the OTHER stack's published port in its environment.
    fs::write(
        log_dir.join("config-alpha.json"),
        serde_json::to_string_pretty(&json!({
            "name": "alpha",
            "services": {
                "api": {
                    "build": { "context": alpha.display().to_string() },
                    "environment": {
                        "SELF_URL": "http://localhost:47510",
                        "PEER_URL": "http://host.docker.internal:47520"
                    },
                    "ports": [
                        {"mode": "ingress", "target": 8080, "published": "47510", "protocol": "tcp"}
                    ]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        log_dir.join("config-beta.json"),
        serde_json::to_string_pretty(&json!({
            "name": "beta",
            "services": {
                "web": {
                    "build": { "context": beta.display().to_string() },
                    "environment": { "API_URL": "http://localhost:47510" },
                    "ports": [
                        {"mode": "ingress", "target": 3000, "published": "47520", "protocol": "tcp"}
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
    assert!(
        output.contains("Stack: alpha"),
        "missing alpha stack: {output}"
    );
    assert!(
        output.contains("Stack: beta"),
        "missing beta stack: {output}"
    );

    // Both stacks started, each as its own per-repo compose project.
    let calls = fs::read_to_string(log_dir.join("calls.log")).unwrap();
    assert!(calls.contains("-p knit-run-venue-capacity--alpha up --build -d"));
    assert!(calls.contains("-p knit-run-venue-capacity--beta up --build -d"));

    let run_dir = workspace.join(".knit/runtime-runs/venue-capacity");
    let alpha_generated: Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("docker-compose.alpha.yml")).unwrap(),
    )
    .unwrap();
    let beta_generated: Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("docker-compose.beta.yml")).unwrap())
            .unwrap();

    // Build contexts remap to each repo's worktree.
    assert!(alpha_generated["services"]["api"]["build"]["context"]
        .as_str()
        .unwrap()
        .ends_with(".knit/worktrees/venue-capacity/alpha"));
    assert!(beta_generated["services"]["web"]["build"]["context"]
        .as_str()
        .unwrap()
        .ends_with(".knit/worktrees/venue-capacity/beta"));

    // Fresh ports per stack.
    let alpha_port = alpha_generated["services"]["api"]["ports"][0]["published"]
        .as_str()
        .unwrap()
        .to_string();
    let beta_port = beta_generated["services"]["web"]["ports"][0]["published"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(alpha_port, "47510");
    assert_ne!(beta_port, "47520");
    assert_ne!(alpha_port, beta_port);

    // Own-stack references rewritten (phase 1) AND cross-stack references
    // rewired to the sibling's NEW bundle port (phase 2).
    let alpha_env = &alpha_generated["services"]["api"]["environment"];
    assert_eq!(
        alpha_env["SELF_URL"],
        format!("http://localhost:{alpha_port}")
    );
    assert_eq!(
        alpha_env["PEER_URL"],
        format!("http://host.docker.internal:{beta_port}")
    );
    assert_eq!(
        beta_generated["services"]["web"]["environment"]["API_URL"],
        format!("http://localhost:{alpha_port}")
    );

    // A repeated `up` owns these listeners through the already-running
    // compose projects. It must replay the bundle's recorded allocations,
    // not interpret its own ports as conflicts and move both stacks.
    fs::write(
        log_dir.join("projects.log"),
        "knit-run-venue-capacity--alpha\nknit-run-venue-capacity--beta\n",
    )
    .unwrap();
    let _listeners = [alpha_port.as_str(), beta_port.as_str()].map(|port| {
        std::net::TcpListener::bind(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap()
    });
    knit_with_env(
        &workspace,
        ["run", "up"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    let alpha_rerun: Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("docker-compose.alpha.yml")).unwrap(),
    )
    .unwrap();
    let beta_rerun: Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("docker-compose.beta.yml")).unwrap())
            .unwrap();
    assert_eq!(
        alpha_rerun["services"]["api"]["ports"][0]["published"],
        alpha_port
    );
    assert_eq!(
        beta_rerun["services"]["web"]["ports"][0]["published"],
        beta_port
    );

    // One run state records every stack.
    let state: Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("state.json")).unwrap()).unwrap();
    let stacks = state["stacks"].as_array().unwrap();
    assert_eq!(stacks.len(), 2);
    assert_eq!(stacks[0]["repo"], "alpha");
    assert_eq!(stacks[0]["projectName"], "knit-run-venue-capacity--alpha");
    assert_eq!(stacks[1]["repo"], "beta");

    // `run down` tears down every stack's compose project.
    fs::remove_file(log_dir.join("calls.log")).unwrap();
    knit_with_env(
        &workspace,
        ["run", "down"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    let calls = fs::read_to_string(log_dir.join("calls.log")).unwrap();
    assert!(calls.contains("-p knit-run-venue-capacity--alpha rm --force --stop --volumes"));
    assert!(calls.contains("-p knit-run-venue-capacity--beta rm --force --stop --volumes"));
    assert!(calls.contains("-p knit-run-venue-capacity--alpha down --remove-orphans"));
    assert!(calls.contains("-p knit-run-venue-capacity--beta down --remove-orphans"));
    assert!(
        calls
            .lines()
            .filter(|line| line.contains(" down "))
            .all(|line| !line.contains("--volumes") && !line.contains("--rmi")),
        "plain `run down` should preserve named restart data and images: {calls}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn run_up_rejects_ambiguous_endpoints_before_starting_stacks() {
    let (root, workspace, fake_bin, log_dir) = setup_ambiguous_runtime();
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let output = knit_fails_with_env(
        &workspace,
        ["run", "up"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    for expected in [
        "Ambiguous cross-stack port references",
        "no application stacks were started",
        "repo `gamma` service `web`",
        "environment key `API_URL`",
        "build args key `API_ORIGIN`",
        "repo `alpha` service `api` -> localhost:",
        "repo `beta` service `api` -> localhost:",
        "source host port 47010",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
    assert!(!output.contains("synthetic-secret"), "{output}");
    assert!(!output.contains("hidden-value"), "{output}");
    let calls = fs::read_to_string(log_dir.join("calls.log")).unwrap_or_default();
    assert!(
        !calls.contains(" up "),
        "application stacks started: {calls}"
    );
    assert!(!workspace
        .join(".knit/runtime-runs/endpoint-validation/state.json")
        .exists());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
fn setup_ambiguous_runtime() -> (
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "demo"]);
    for repo in ["alpha", "beta", "gamma"] {
        let source = root.join(repo);
        init_repo(&source, repo);
        knit(
            &workspace,
            ["project", "add", repo, source.to_str().unwrap()],
        );
    }
    knit(&workspace, ["bundle", "endpoint validation"]);
    for repo in ["alpha", "beta", "gamma"] {
        fs::write(
            workspace.join(format!(
                ".knit/worktrees/endpoint-validation/{repo}/docker-compose.yml"
            )),
            "services:\n  app:\n    image: scratch\n",
        )
        .unwrap();
    }
    let (fake_bin, log_dir) = write_fake_docker_multi(&root);
    for repo in ["alpha", "beta"] {
        fs::write(
            log_dir.join(format!("config-{repo}.json")),
            serde_json::to_string(&json!({"services": {"api": {
                "image": "scratch",
                "ports": [{"target": 8000, "published": "47010", "protocol": "tcp"}]
            }}}))
            .unwrap(),
        )
        .unwrap();
    }
    fs::write(
        log_dir.join("config-gamma.json"),
        serde_json::to_string(&json!({"services": {"web": {
            "build": {
                "context": root.join("gamma").display().to_string(),
                "args": {"API_ORIGIN": "http://127.0.0.1:47010"}
            },
            "environment": {
                "API_URL": "http://user:synthetic-secret@localhost:47010/api?token=hidden-value"
            }
        }}}))
        .unwrap(),
    )
    .unwrap();
    (root, workspace, fake_bin, log_dir)
}

#[cfg(unix)]
fn write_endpoint_binding(workspace: &Path, target_repo: &str) {
    let path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    project["runtime"] = json!({"bindings": [
        {"repo": "gamma", "service": "web", "environment": "API_URL",
         "target": {"repo": target_repo, "service": "api", "port": 8000}},
        {"repo": "gamma", "service": "web", "buildArg": "API_ORIGIN",
         "target": {"repo": target_repo, "service": "api"}}
    ]});
    fs::write(path, serde_json::to_string_pretty(&project).unwrap()).unwrap();
}

#[cfg(unix)]
fn generated_runtime_config(workspace: &Path, bundle: &str, repo: &str) -> Value {
    serde_json::from_str(
        &fs::read_to_string(workspace.join(format!(
            ".knit/runtime-runs/{bundle}/docker-compose.{repo}.yml"
        )))
        .unwrap(),
    )
    .unwrap()
}

#[cfg(unix)]
#[test]
fn run_up_binds_services_across_colliding_ports_and_parallel_bundles() {
    let (root, workspace, fake_bin, log_dir) = setup_ambiguous_runtime();
    write_endpoint_binding(&workspace, "beta");
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let env = [
        ("PATH", path.as_str()),
        ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
    ];
    knit_with_env(&workspace, ["run", "up"], &env);
    let assert_binding = |bundle: &str| -> String {
        let target = generated_runtime_config(&workspace, bundle, "beta");
        let port = target["services"]["api"]["ports"][0]["published"]
            .as_str()
            .unwrap()
            .to_string();
        let other = generated_runtime_config(&workspace, bundle, "alpha");
        assert_ne!(other["services"]["api"]["ports"][0]["published"], port);
        let consumer = generated_runtime_config(&workspace, bundle, "gamma");
        assert_eq!(
            consumer["services"]["web"]["environment"]["API_URL"],
            format!("http://user:synthetic-secret@localhost:{port}/api?token=hidden-value")
        );
        assert_eq!(
            consumer["services"]["web"]["build"]["args"]["API_ORIGIN"],
            format!("http://127.0.0.1:{port}")
        );
        port
    };
    let first_port = assert_binding("endpoint-validation");
    fs::write(log_dir.join("projects.log"),
        "knit-run-endpoint-validation--alpha\nknit-run-endpoint-validation--beta\nknit-run-endpoint-validation--gamma\n").unwrap();
    knit(&workspace, ["bundle", "parallel endpoints"]);
    for repo in ["alpha", "beta", "gamma"] {
        fs::write(
            workspace.join(format!(
                ".knit/worktrees/parallel-endpoints/{repo}/docker-compose.yml"
            )),
            "services:\n  app:\n    image: scratch\n",
        )
        .unwrap();
    }
    knit_with_env(&workspace, ["run", "up"], &env);
    assert_ne!(assert_binding("parallel-endpoints"), first_port);
    knit_with_env(
        &workspace,
        ["--bundle", "endpoint-validation", "run", "up"],
        &env,
    );
    assert_eq!(assert_binding("endpoint-validation"), first_port);
    let calls = fs::read_to_string(log_dir.join("calls.log")).unwrap();
    let first_wait = calls.find(" ps ").expect("startup verification missing");
    assert_eq!(
        calls[..first_wait].matches("up --build -d").count(),
        3,
        "all dependent stacks must launch before readiness checks: {calls}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn run_up_keeps_unrelated_same_named_database() {
    let (root, workspace, fake_bin, log_dir) = setup_ambiguous_runtime();
    write_endpoint_binding(&workspace, "alpha");
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let shared_port = listener.local_addr().unwrap().port();
    let path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    project["runtime"]["database"] = json!({
        "mode":"shared", "service":"db", "name":"main_dev", "port":shared_port
    });
    fs::write(path, serde_json::to_string_pretty(&project).unwrap()).unwrap();
    for (repo, name) in [("alpha", "main_dev"), ("beta", "independent_dev")] {
        let path = log_dir.join(format!("config-{repo}.json"));
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        config["services"]["db"] = json!({"image":"postgres:17",
            "environment":{"POSTGRES_DB":name,"POSTGRES_USER":"dev","POSTGRES_PASSWORD":"synthetic"},
            "volumes":[{"type":"volume","source":"postgres_data","target":"/var/lib/postgresql/data"}],
            "ports":[{"published":shared_port.to_string(),"target":5432,"protocol":"tcp"}]});
        config["volumes"] = json!({"postgres_data":{}});
        config["services"]["api"]["environment"] = json!({
            "DATABASE_URL":format!("postgres://dev:synthetic@db:5432/{name}"),
            "INDEPENDENT_URL":format!("postgres://host.docker.internal:{shared_port}/independent_dev")});
        config["services"]["api"]["depends_on"] = json!({"db":{"condition":"service_healthy"}});
        fs::write(path, serde_json::to_string(&config).unwrap()).unwrap();
    }
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    knit_with_env(
        &workspace,
        ["run", "up"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    let main = generated_runtime_config(&workspace, "endpoint-validation", "alpha");
    assert!(main["services"].get("db").is_none());
    assert_eq!(
        main["services"]["api"]["environment"]["DATABASE_URL"],
        format!("postgres://dev:synthetic@host.docker.internal:{shared_port}/main_dev")
    );
    let separate = generated_runtime_config(&workspace, "endpoint-validation", "beta");
    assert_eq!(
        separate["services"]["db"]["environment"]["POSTGRES_DB"],
        "independent_dev"
    );
    let independent_port = separate["services"]["db"]["ports"][0]["published"]
        .as_str()
        .unwrap();
    assert_eq!(
        main["services"]["api"]["environment"]["INDEPENDENT_URL"],
        format!("postgres://host.docker.internal:{independent_port}/independent_dev")
    );
    assert!(separate["volumes"].get("postgres_data").is_some());
    assert_eq!(
        separate["services"]["api"]["environment"]["DATABASE_URL"],
        "postgres://dev:synthetic@db:5432/independent_dev"
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn run_up_does_not_record_success_when_service_crashes() {
    let (root, workspace, fake_bin, log_dir) = setup_ambiguous_runtime();
    write_endpoint_binding(&workspace, "beta");
    fs::write(
        log_dir.join("status.json"),
        r#"[{"Service":"api","State":"exited","Health":"","ExitCode":1}]"#,
    )
    .unwrap();
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let output = knit_fails_with_env(
        &workspace,
        ["run", "up"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    assert!(output.contains("exited"), "{output}");
    assert!(!workspace
        .join(".knit/runtime-runs/endpoint-validation/state.json")
        .exists());
    let allocation_path = workspace.join(".knit/runtime-runs/endpoint-validation/allocations.json");
    let before: Value =
        serde_json::from_str(&fs::read_to_string(&allocation_path).unwrap()).unwrap();
    let listeners: Vec<_> = before["ports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|port| {
            std::net::TcpListener::bind(("127.0.0.1", port["host"].as_u64().unwrap() as u16))
                .unwrap()
        })
        .collect();
    fs::write(log_dir.join("projects.log"),
        "knit-run-endpoint-validation--alpha\nknit-run-endpoint-validation--beta\nknit-run-endpoint-validation--gamma\n").unwrap();
    fs::remove_file(log_dir.join("status.json")).unwrap();
    knit_with_env(
        &workspace,
        ["run", "up"],
        &[
            ("PATH", path.as_str()),
            ("FAKE_DOCKER_DIR", log_dir.to_str().unwrap()),
        ],
    );
    let after: Value =
        serde_json::from_str(&fs::read_to_string(&allocation_path).unwrap()).unwrap();
    assert_eq!(
        before["ports"], after["ports"],
        "retry changed allocated ports"
    );
    drop(listeners);
    fs::remove_dir_all(root).unwrap();
}
