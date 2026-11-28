//! What bundle uploads carry about the machine. Ledger nodes name the
//! machine that recorded them with a random label kept in the global Knit
//! config directory, never the real hostname, and nodes an older Knit
//! recorded here under the real hostname still count as this machine.
//! Checkout paths only travel to a sync remote on this same computer, and a
//! pull of an artifact without them keeps the local ones.

mod common;
use common::*;
use serde_json::{json, Value};
use std::{fs, path::Path};

/// Run knit with a chosen KNIT_HOME and a fixed `HOSTNAME`, the first source
/// older Knit versions read the hostname from.
fn run(cwd: &Path, home: &Path, args: &[&str], extra: &[(&str, &str)]) -> String {
    let mut env = vec![
        ("KNIT_HOME", home.to_str().unwrap()),
        ("HOSTNAME", "real-host-name"),
        ("KNIT_ENVIRONMENT_ID", ""),
        ("KNIT_MACHINE_LABEL", ""),
    ];
    env.extend_from_slice(extra);
    let (stdout, stderr, ok) = knit_split_output(cwd, args, &env);
    assert!(ok, "knit {args:?} failed: {stderr}\n{stdout}");
    stdout
}

fn bundle(workspace: &Path) -> Value {
    serde_json::from_str(
        &fs::read_to_string(workspace.join(".knit/bundles/travel.bundle.json")).unwrap(),
    )
    .unwrap()
}

fn workspace_with_commit(home: &Path, extra: &[(&str, &str)]) -> std::path::PathBuf {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    run(&workspace, home, &["bundle", "travel"], extra);
    run(
        &workspace,
        home,
        &["bundle", "add", backend.to_str().unwrap()],
        extra,
    );
    fs::write(
        workspace.join(".knit/worktrees/travel/backend/app.txt"),
        "feature\n",
    )
    .unwrap();
    run(
        &workspace,
        home,
        &["commit", "--all", "-m", "Add feature"],
        extra,
    );
    workspace
}

#[test]
fn new_nodes_record_a_stable_random_label_instead_of_the_hostname() {
    let home = unique_temp_dir().join("home");
    let workspace = workspace_with_commit(&home, &[]);
    let id = fs::read_to_string(home.join("machine-id")).unwrap();
    let id = id.trim();
    assert_eq!(id.len(), 32, "{id}");
    let label = format!("machine-{}", &id[..12]);

    let recorded = bundle(&workspace);
    let nodes = recorded["nodes"].as_array().unwrap();
    assert!(nodes.len() >= 2);
    for node in nodes {
        assert_eq!(node["origin"]["hostname"], json!(label), "{node}");
        assert!(!node["origin"]["platform"].as_str().unwrap().is_empty());
    }
    let text = fs::read_to_string(workspace.join(".knit/bundles/travel.bundle.json")).unwrap();
    assert!(!text.contains("real-host-name"));

    // Validation still finds hostname and platform on every node.
    run(&workspace, &home, &["bundle", "validate"], &[]);
}

#[test]
fn an_explicit_machine_label_replaces_the_random_one() {
    let home = unique_temp_dir().join("home");
    let workspace = workspace_with_commit(&home, &[("KNIT_MACHINE_LABEL", "studio")]);
    for node in bundle(&workspace)["nodes"].as_array().unwrap() {
        assert_eq!(node["origin"]["hostname"], json!("studio"));
    }
}

/// Append a `handoff.in` node, as if accepted on the machine named
/// `hostname`, so the bundle reads as active there.
fn record_handoff_in(workspace: &Path, hostname: &str) {
    let path = workspace.join(".knit/bundles/travel.bundle.json");
    let mut value = bundle(workspace);
    let platform = value["nodes"][0]["origin"]["platform"].clone();
    let origin = json!({"hostname": hostname, "platform": platform});
    let nodes = value["nodes"].as_array_mut().unwrap();
    nodes.push(json!({
        "id": "hin_legacy",
        "type": "handoff.in",
        "createdAt": "2030-01-01T00:00:00Z",
        "origin": origin,
        "handoff": {"id": "handoff_legacy", "source": origin, "outNodeId": "hout_legacy"},
        "repoIds": ["backend"]
    }));
    value["headNodeId"] = json!("hin_legacy");
    fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

#[test]
fn nodes_older_knit_recorded_here_under_the_hostname_still_count_as_this_machine() {
    let home = unique_temp_dir().join("home");
    let workspace = workspace_with_commit(&home, &[]);
    record_handoff_in(&workspace, "real-host-name");

    let status = run(&workspace, &home, &["handoff", "status"], &[]);
    assert!(
        status.contains("Continued on this machine (real-host-name)"),
        "{status}"
    );

    // The same ledger read on another machine names the recorded host.
    let elsewhere = run(
        &workspace,
        &home,
        &["handoff", "status"],
        &[("HOSTNAME", "other-host")],
    );
    assert!(
        elsewhere.contains("Continued on real-host-name"),
        "{elsewhere}"
    );
    assert!(!elsewhere.contains("this machine"), "{elsewhere}");

    // Reading never rewrites the recorded node.
    let recorded = bundle(&workspace);
    let node = recorded["nodes"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(node["origin"]["hostname"], json!("real-host-name"));
}

#[test]
fn nodes_recorded_under_this_machines_label_count_as_this_machine() {
    let home = unique_temp_dir().join("home");
    let workspace = workspace_with_commit(&home, &[]);
    let label = bundle(&workspace)["nodes"][0]["origin"]["hostname"]
        .as_str()
        .unwrap()
        .to_string();
    record_handoff_in(&workspace, &label);
    let status = run(&workspace, &home, &["handoff", "status"], &[]);
    assert!(
        status.contains(&format!("Continued on this machine ({label})")),
        "{status}"
    );
    // Another machine (another config directory, so another label) does not.
    let other_home = unique_temp_dir().join("home");
    let elsewhere = run(&workspace, &other_home, &["handoff", "status"], &[]);
    assert!(
        elsewhere.contains(&format!("Continued on {label}")),
        "{elsewhere}"
    );
    assert!(!elsewhere.contains("this machine"), "{elsewhere}");
}

/// A workspace with project repo `backend` (its origin a local bare repo) and
/// one commit on bundle `travel`.
fn project_workspace() -> (std::path::PathBuf, std::path::PathBuf) {
    let root = unique_temp_dir();
    let (_remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(&workspace, ["bundle", "travel", "--repo", "backend"]);
    append_line(
        &workspace.join(".knit/worktrees/travel/backend/app.txt"),
        "feature",
    );
    knit(&workspace, ["commit", "--all", "-m", "Feature"]);
    (root, workspace)
}

fn last_uploaded_payload(api: &Path) -> Value {
    let bodies = fs::read_to_string(api.join("artifact-travel.bodies")).unwrap();
    let last: Value = serde_json::from_str(bodies.lines().last().unwrap()).unwrap();
    last["payload"].clone()
}

#[test]
fn pushes_to_another_computer_carry_no_checkout_paths_or_hostname() {
    let Some(address) = non_loopback_address() else {
        eprintln!("skipped: this machine has no non-loopback address");
        return;
    };
    let (root, workspace) = project_workspace();
    let api = root.join("api");
    let url = relay(address, &spawn_fake_remote_push_api(&api));
    knit(&workspace, ["remote", "add", "hosted", &url]);
    let env = [
        ("KNIT_REMOTE_TOKEN", "test-token"),
        ("HOSTNAME", "real-host-name"),
    ];
    knit_with_env(&workspace, ["push", "--set-upstream"], &env);

    let uploaded = last_uploaded_payload(&api);
    let repo = &uploaded["repos"][0];
    assert_eq!(repo["path"], json!(""), "{repo}");
    assert!(
        repo.get("worktreePath").is_none_or(Value::is_null),
        "{repo}"
    );
    assert!(
        repo["featureBranch"].is_string() && repo["headSha"].is_string(),
        "{repo}"
    );
    let text = uploaded.to_string();
    // (`remote` names the origin, here a local bare repository; real
    // projects name a forge URL.)
    assert!(!text.contains(workspace.to_str().unwrap()), "{text}");
    assert!(
        !text.contains(&format!("{}\"", root.join("backend").display())),
        "{text}"
    );
    assert!(!text.contains("real-host-name"), "{text}");
    for node in uploaded["nodes"].as_array().unwrap() {
        let hostname = node["origin"]["hostname"].as_str().unwrap();
        assert!(hostname.starts_with("machine-"), "{node}");
    }

    // The local artifact keeps what this machine needs.
    let local = bundle(&workspace);
    assert!(!local["repos"][0]["path"].as_str().unwrap().is_empty());
    assert!(local["repos"][0]["worktreePath"].is_string());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pushes_to_a_server_on_this_computer_keep_checkout_paths() {
    let (root, workspace) = project_workspace();
    let api = root.join("api");
    let url = spawn_fake_remote_push_api(&api);
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    knit(&workspace, ["remote", "add", "hosted", &url]);
    knit_with_env(
        &workspace,
        ["push", "--set-upstream"],
        &[("KNIT_REMOTE_TOKEN", "test-token")],
    );
    let uploaded = last_uploaded_payload(&api);
    let local = bundle(&workspace);
    // A local server's review analysis reads these folders directly.
    assert_eq!(uploaded["repos"][0]["path"], local["repos"][0]["path"]);
    assert_eq!(
        uploaded["repos"][0]["worktreePath"],
        local["repos"][0]["worktreePath"]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pulling_an_artifact_without_paths_keeps_the_local_ones() {
    let (root, workspace) = project_workspace();
    // Publish the feature branch to origin (git only: no sync remote yet).
    knit(&workspace, ["push", "--set-upstream"]);
    let local = bundle(&workspace);
    let local_path = local["repos"][0]["path"].clone();
    let local_worktree = local["repos"][0]["worktreePath"].clone();
    assert!(local_worktree.is_string());

    // The server copy as a new Knit uploads it, one node ahead (another
    // machine's checkpoint).
    let mut remote = local.clone();
    remote["repos"][0]["path"] = json!("");
    remote["repos"][0]
        .as_object_mut()
        .unwrap()
        .remove("worktreePath");
    let platform = remote["nodes"][0]["origin"]["platform"].clone();
    remote["nodes"].as_array_mut().unwrap().push(json!({
        "id": "chk_elsewhere",
        "type": "checkpoint",
        "createdAt": "2030-01-01T00:00:00Z",
        "message": "From the other machine",
        "origin": {"hostname": "machine-0123456789ab", "platform": platform}
    }));
    remote["headNodeId"] = json!("chk_elsewhere");
    let export = json!({
        "data": {
            "project": {"slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-travel",
                "slug": "travel",
                "lifecycleState": "open",
                "currentArtifact": {"artifactHash": "sha256:remote", "payload": remote},
            }],
            "historyEvents": [],
        }
    });
    let url = spawn_fake_remote_with_body(export.to_string());
    knit(&workspace, ["remote", "add", "hosted", &url]);
    let output = knit_with_env(&workspace, ["pull"], &[("KNIT_REMOTE_TOKEN", "test-token")]);
    assert!(output.contains("travel pulled"), "{output}");

    let pulled = bundle(&workspace);
    assert!(
        pulled["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == "chk_elsewhere"),
        "{pulled}"
    );
    assert_eq!(pulled["repos"][0]["path"], local_path);
    assert_eq!(pulled["repos"][0]["worktreePath"], local_worktree);
    // The other machine's node keeps its label as recorded.
    let node = pulled["nodes"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(node["origin"]["hostname"], json!("machine-0123456789ab"));
    knit(&workspace, ["bundle", "validate"]);
    fs::remove_dir_all(root).unwrap();
}
