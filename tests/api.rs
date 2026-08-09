mod common;

use common::{knit, setup_three_repo_project, unique_temp_dir};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn describe_exposes_the_stable_action_catalog() {
    let root = unique_temp_dir();
    let output = knit(&root, ["api", "describe"]);
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["apiVersion"], "1.0");
    assert_eq!(value["actions"].as_array().unwrap().len(), 51);
    let delete = value["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["id"] == "bundle.delete")
        .unwrap();
    assert_eq!(delete["destructive"], true);
    assert_eq!(delete["requiresConfirmation"], true);
}

#[test]
fn snapshot_reads_workspace_artifacts_without_cli_text_parsing() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    knit(&workspace, ["bundle", "api fixture", "--offline"]);

    let output = knit(&workspace, ["api", "snapshot", "--bundle", "api-fixture"]);
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["isKnit"], true);
    assert_eq!(value["activeBundle"], "api-fixture");
    assert!(value["projects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|project| project["id"] == "demo"));
    assert!(value["bundles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|bundle| bundle["id"] == "api-fixture"));
    assert_eq!(value["artifactErrors"], json!([]));
}

#[test]
fn action_run_pins_bundle_and_forwards_only_the_explicit_session() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    setup_three_repo_project(&workspace, &root);
    knit(&workspace, ["bundle", "session fixture", "--offline"]);

    let output = knit(
        &workspace,
        [
            "api",
            "run",
            "check.record",
            "--bundle",
            "session-fixture",
            "--session-id",
            "agent-session-123",
            "--input-json",
            r#"{"name":"api","verdict":"pass"}"#,
        ],
    );
    let events = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.last().unwrap()["kind"], "result");

    let bundle: Value = serde_json::from_str(
        &std::fs::read_to_string(workspace.join(".knit/bundles/session-fixture.bundle.json"))
            .unwrap(),
    )
    .unwrap();
    let node = bundle["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(node["sessionId"], "agent-session-123");
}

#[test]
fn json_rpc_and_mcp_catalogs_share_the_native_actions() {
    let api = exchange(
        &["api", "serve", "--stdio"],
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"action.list","params":{}}),
        ],
    );
    assert_eq!(
        api[0]["result"]["capabilities"]["actionCancellation"],
        false
    );
    assert_eq!(api[1]["result"]["actions"].as_array().unwrap().len(), 51);

    let mcp = exchange(
        &["mcp", "--stdio"],
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        ],
    );
    assert_eq!(mcp[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(mcp[1]["result"]["tools"].as_array().unwrap().len(), 52);
}

#[test]
fn json_rpc_distinguishes_invalid_action_params_from_unknown_methods() {
    let responses = exchange(
        &["api", "serve", "--stdio"],
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"action.run","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"unknown.method","params":{}}),
        ],
    );
    assert_eq!(responses[0]["error"]["code"], -32602);
    assert_eq!(responses[1]["error"]["code"], -32601);
}

fn exchange(args: &[&str], messages: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_knit"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for message in messages {
            writeln!(stdin, "{message}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
