//! Stable machine interfaces for Knit SDKs and agent harnesses.
//!
//! The action catalog is intentionally owned by the CLI. SDKs discover this
//! catalog and submit structured inputs instead of duplicating command-line
//! construction. Every execution uses an argv array and an explicit cwd; no
//! action is evaluated by a shell.

use crate::cli::ApiCommand;
use crate::store::{find_knit_root, load_config};
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_VERSION: &str = "1.0";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Clone, Copy)]
struct ParamDef {
    key: &'static str,
    label: &'static str,
    kind: &'static str,
    required: bool,
}

#[derive(Clone, Copy)]
struct ActionDef {
    id: &'static str,
    title: &'static str,
    category: &'static str,
    scope: &'static str,
    read_only: bool,
    destructive: bool,
    open_world: bool,
    description: &'static str,
    params: &'static [ParamDef],
}

const fn p(key: &'static str, label: &'static str, kind: &'static str, required: bool) -> ParamDef {
    ParamDef {
        key,
        label,
        kind,
        required,
    }
}

const NONE: &[ParamDef] = &[];
const MESSAGE: &[ParamDef] = &[p("message", "Message", "string", true)];
const ARCHIVE: &[ParamDef] = &[
    p("reason", "Reason", "string", false),
    p("force", "Force", "boolean", false),
    p("keepWorktrees", "Keep worktrees", "boolean", false),
];
const REPOS: &[ParamDef] = &[p("repos", "Repositories", "array", true)];
const REMOVE_REPOS: &[ParamDef] = &[
    p("repos", "Repositories", "array", true),
    p("force", "Force", "boolean", false),
    p("keepWorktree", "Keep worktree", "boolean", false),
];
const SLUG: &[ParamDef] = &[p("slug", "Bundle", "string", true)];
const CHECK_RUN: &[ParamDef] = &[p("name", "Check", "string", true)];
const CHECK_RECORD: &[ParamDef] = &[
    p("name", "Check", "string", true),
    p("verdict", "Verdict", "string", true),
    p("detail", "Detail", "string", false),
];
const TAG: &[ParamDef] = &[
    p("name", "Name", "string", true),
    p("message", "Note", "string", false),
];
const PUBLISH: &[ParamDef] = &[
    p("base", "Target branch", "string", false),
    p("draft", "Draft reviews", "boolean", false),
];
const LAND_TARGET: &[ParamDef] = &[p("target", "Target branch", "string", false)];
const LAND_APPLY: &[ParamDef] = &[
    p("target", "Target branch", "string", false),
    p("skipChecks", "Skip required checks", "boolean", false),
];
const APPLY: &[ParamDef] = &[p("apply", "Apply", "boolean", false)];
const PUSH: &[ParamDef] = &[
    p("message", "Commit message", "string", true),
    p("setUpstream", "Set upstream", "boolean", false),
];
const PULL: &[ParamDef] = &[
    p("rebase", "Rebase", "boolean", false),
    p("main", "Update base branches", "boolean", false),
];
const MERGE: &[ParamDef] = &[
    p("into", "Target branch", "string", true),
    p("push", "Push", "boolean", false),
];
const PICK: &[ParamDef] = &[
    p("from", "Source bundle", "string", true),
    p("targets", "Targets", "array", true),
];
const TARGET: &[ParamDef] = &[p("target", "Target", "string", true)];
const CREATE: &[ParamDef] = &[
    p("title", "Title", "string", true),
    p("view", "View", "string", false),
    p("repos", "Repositories", "array", false),
];
const REMOTE_ADD: &[ParamDef] = &[
    p("name", "Name", "string", true),
    p("url", "URL", "string", true),
    p("global", "Global", "boolean", false),
];
const GLOBAL: &[ParamDef] = &[p("global", "Global", "boolean", false)];
const REMOTE_REMOVE: &[ParamDef] = &[
    p("name", "Name", "string", true),
    p("global", "Global", "boolean", false),
];
const REMOTE_PROJECTS: &[ParamDef] = &[p("remote", "Remote", "string", false)];
const CLONE: &[ParamDef] = &[
    p("project", "Project", "string", true),
    p("target", "Target folder", "string", true),
];
const LIMIT: &[ParamDef] = &[p("limit", "Limit", "string", false)];
const PRUNE: &[ParamDef] = &[
    p("worktrees", "Remove worktrees", "boolean", false),
    p("branches", "Delete branches", "boolean", false),
    p("all", "Remove everything", "boolean", false),
];

macro_rules! action {
    ($id:literal, $title:literal, $category:literal, $scope:literal, $ro:literal, $destructive:literal, $open:literal, $params:ident) => {
        ActionDef {
            id: $id,
            title: $title,
            category: $category,
            scope: $scope,
            read_only: $ro,
            destructive: $destructive,
            open_world: $open,
            description: $title,
            params: $params,
        }
    };
}

// Compatibility boundary with the original @t3tools/knit-typescript-sdk
// action catalog. IDs are stable; additions require an API minor bump.
const ACTIONS: &[ActionDef] = &[
    action!(
        "commit",
        "Commit all repositories",
        "Lifecycle",
        "bundle",
        false,
        false,
        false,
        MESSAGE
    ),
    action!(
        "bundle.archive",
        "Archive bundle",
        "Lifecycle",
        "bundle",
        false,
        true,
        false,
        ARCHIVE
    ),
    action!(
        "bundle.delete",
        "Delete bundle",
        "Lifecycle",
        "bundle",
        false,
        true,
        false,
        NONE
    ),
    action!(
        "bundle.validate",
        "Validate bundle",
        "Lifecycle",
        "bundle",
        true,
        false,
        false,
        NONE
    ),
    action!(
        "bundle.add",
        "Add repositories",
        "Lifecycle",
        "bundle",
        false,
        false,
        true,
        REPOS
    ),
    action!(
        "bundle.remove",
        "Remove repositories",
        "Lifecycle",
        "bundle",
        false,
        true,
        false,
        REMOVE_REPOS
    ),
    action!(
        "bundle.worktree",
        "Materialize worktrees",
        "Lifecycle",
        "bundle",
        false,
        false,
        true,
        NONE
    ),
    action!(
        "bundle.pull",
        "Pull bundle",
        "Lifecycle",
        "workspace",
        false,
        false,
        true,
        SLUG
    ),
    action!(
        "status",
        "Status",
        "Lifecycle",
        "bundle",
        true,
        false,
        false,
        NONE
    ),
    action!(
        "run.up",
        "Start runtime",
        "Runtime",
        "bundle",
        false,
        false,
        true,
        NONE
    ),
    action!(
        "run.status",
        "Runtime status",
        "Runtime",
        "bundle",
        true,
        false,
        false,
        NONE
    ),
    action!(
        "run.down",
        "Stop runtime",
        "Runtime",
        "bundle",
        false,
        true,
        false,
        NONE
    ),
    action!(
        "check.run",
        "Run check",
        "Checks",
        "bundle",
        false,
        false,
        true,
        CHECK_RUN
    ),
    action!(
        "check.record",
        "Record check verdict",
        "Checks",
        "bundle",
        false,
        false,
        false,
        CHECK_RECORD
    ),
    action!(
        "tag.create",
        "Tag known-good",
        "Checks",
        "bundle",
        false,
        false,
        true,
        TAG
    ),
    action!(
        "publish.create",
        "Create review group",
        "Publish",
        "bundle",
        false,
        false,
        true,
        PUBLISH
    ),
    action!(
        "publish.sync",
        "Sync review group",
        "Publish",
        "bundle",
        false,
        false,
        true,
        NONE
    ),
    action!(
        "publish.status",
        "Review group status",
        "Publish",
        "bundle",
        true,
        false,
        true,
        NONE
    ),
    action!(
        "land.plan",
        "Generate landing plan",
        "Land",
        "bundle",
        false,
        false,
        true,
        LAND_TARGET
    ),
    action!(
        "land.check",
        "Landing preflight",
        "Land",
        "bundle",
        true,
        false,
        true,
        NONE
    ),
    action!(
        "land.apply",
        "Apply landing plan",
        "Land",
        "bundle",
        false,
        true,
        true,
        LAND_APPLY
    ),
    action!(
        "land.ship",
        "Plan and apply landing",
        "Land",
        "bundle",
        false,
        true,
        true,
        LAND_APPLY
    ),
    action!(
        "land.status",
        "Landing status",
        "Land",
        "bundle",
        true,
        false,
        false,
        NONE
    ),
    action!(
        "land.resume",
        "Resume landing",
        "Land",
        "bundle",
        false,
        false,
        true,
        NONE
    ),
    action!(
        "land.rollback",
        "Rollback landing",
        "Land",
        "bundle",
        false,
        true,
        true,
        APPLY
    ),
    action!(
        "push",
        "Commit and push",
        "Git",
        "bundle",
        false,
        false,
        true,
        PUSH
    ),
    action!("pull", "Pull", "Git", "bundle", false, false, true, PULL),
    action!(
        "fetch",
        "Fetch project",
        "Git",
        "workspace",
        false,
        false,
        true,
        NONE
    ),
    action!(
        "diff.stat",
        "Diff stat",
        "Git",
        "bundle",
        true,
        false,
        false,
        NONE
    ),
    action!(
        "merge",
        "Merge bundle",
        "Merge",
        "bundle",
        false,
        true,
        true,
        MERGE
    ),
    action!(
        "merge.continue",
        "Continue merge",
        "Merge",
        "bundle",
        false,
        false,
        false,
        NONE
    ),
    action!(
        "merge.abort",
        "Abort merge",
        "Merge",
        "bundle",
        false,
        true,
        false,
        NONE
    ),
    action!(
        "cherrypick.dryrun",
        "Preview cherry-pick",
        "Merge",
        "workspace",
        true,
        false,
        false,
        PICK
    ),
    action!(
        "cherrypick.apply",
        "Apply cherry-pick",
        "Merge",
        "workspace",
        false,
        true,
        false,
        PICK
    ),
    action!(
        "revert.plan",
        "Plan revert",
        "Merge",
        "bundle",
        true,
        false,
        false,
        TARGET
    ),
    action!(
        "revert.apply",
        "Apply revert",
        "Merge",
        "bundle",
        false,
        true,
        false,
        TARGET
    ),
    action!(
        "bundle.create",
        "Create bundle",
        "Workspace",
        "workspace",
        false,
        false,
        true,
        CREATE
    ),
    action!(
        "remote.add",
        "Add remote",
        "Remotes",
        "workspace",
        false,
        false,
        true,
        REMOTE_ADD
    ),
    action!(
        "remote.list",
        "List remotes",
        "Remotes",
        "workspace",
        true,
        false,
        false,
        GLOBAL
    ),
    action!(
        "remote.remove",
        "Remove remote",
        "Remotes",
        "workspace",
        false,
        true,
        true,
        REMOTE_REMOVE
    ),
    action!(
        "remote.projects",
        "List remote projects",
        "Remotes",
        "workspace",
        true,
        false,
        true,
        REMOTE_PROJECTS
    ),
    action!(
        "clone",
        "Clone project",
        "Remotes",
        "workspace",
        false,
        false,
        true,
        CLONE
    ),
    action!(
        "sync.push",
        "Sync push",
        "Workspace",
        "workspace",
        false,
        false,
        true,
        NONE
    ),
    action!(
        "sync.pull",
        "Sync pull",
        "Workspace",
        "workspace",
        false,
        false,
        true,
        NONE
    ),
    action!(
        "sync.pushKg",
        "Push knowledge graph",
        "Workspace",
        "workspace",
        false,
        false,
        true,
        NONE
    ),
    action!(
        "history.list",
        "List history",
        "Workspace",
        "workspace",
        true,
        false,
        false,
        LIMIT
    ),
    action!(
        "prune.report",
        "Prune report",
        "Maintenance",
        "workspace",
        true,
        false,
        true,
        NONE
    ),
    action!(
        "prune.apply",
        "Apply prune",
        "Maintenance",
        "workspace",
        false,
        true,
        true,
        PRUNE
    ),
    action!(
        "doctor",
        "Doctor",
        "Maintenance",
        "workspace",
        true,
        false,
        false,
        NONE
    ),
    action!(
        "project.show",
        "Show project",
        "Maintenance",
        "workspace",
        true,
        false,
        false,
        NONE
    ),
    action!(
        "view.list",
        "List views",
        "Maintenance",
        "workspace",
        true,
        false,
        false,
        NONE
    ),
];

pub fn run_api(command: ApiCommand) -> Result<()> {
    match command {
        ApiCommand::Describe => print_json(&describe()),
        ApiCommand::Snapshot { workspace, bundle } => {
            print_json(&snapshot(workspace.as_deref(), bundle.as_deref())?)
        }
        ApiCommand::Watch {
            workspace,
            bundle,
            interval_ms,
            once,
        } => watch(workspace.as_deref(), bundle.as_deref(), interval_ms, once),
        ApiCommand::Run {
            action_id,
            workspace,
            bundle,
            session_id,
            input_json,
        } => {
            let input = read_input(&input_json)?;
            let events = execute_action(
                &action_id,
                workspace.as_deref(),
                bundle.as_deref(),
                session_id.as_deref(),
                &input,
            )?;
            for event in events {
                print_json(&event)?;
            }
            Ok(())
        }
        ApiCommand::Serve { stdio } => serve_api(stdio),
    }
}

pub fn serve_mcp(stdio: bool) -> Result<()> {
    if !stdio {
        bail!("only --stdio transport is currently supported");
    }
    serve_lines(handle_mcp_request)
}

fn serve_api(stdio: bool) -> Result<()> {
    if !stdio {
        bail!("only --stdio transport is currently supported");
    }
    serve_lines(handle_api_request)
}

pub fn describe() -> Value {
    json!({
        "apiVersion": API_VERSION,
        "protocol": { "name": "knit.api", "jsonRpc": "2.0", "framing": "ndjson" },
        "actions": ACTIONS.iter().map(action_descriptor).collect::<Vec<_>>()
    })
}

fn action_descriptor(action: &ActionDef) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut legacy_params = Vec::new();
    for param in action.params {
        let schema = match param.kind {
            "boolean" => json!({"type": "boolean"}),
            "array" => json!({"type": "array", "items": {"type": "string"}}),
            _ => json!({"type": "string"}),
        };
        properties.insert(param.key.to_string(), schema);
        if param.required {
            required.push(param.key);
        }
        legacy_params.push(json!({
            "key": param.key,
            "label": param.label,
            "type": if param.kind == "array" { "repos" } else if param.kind == "string" { "text" } else { param.kind },
            "required": param.required
        }));
    }
    json!({
        "id": action.id,
        "title": action.title,
        "description": action.description,
        "category": action.category,
        "scope": action.scope,
        "readOnly": action.read_only,
        "readonly": action.read_only,
        "destructive": action.destructive,
        "openWorld": action.open_world,
        "requiresConfirmation": action.destructive,
        "minimumApiVersion": API_VERSION,
        "params": legacy_params,
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": properties,
            "required": required
        }
    })
}

pub fn snapshot(workspace: Option<&Path>, bundle: Option<&str>) -> Result<Value> {
    let start = match workspace {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("failed to read current directory")?,
    };
    let start = fs::canonicalize(&start).unwrap_or(start);
    let Some(root) = find_knit_root(&start) else {
        return Ok(json!({
            "apiVersion": API_VERSION,
            "workspaceRoot": start,
            "isKnit": false,
            "activeBundle": null,
            "activeProject": null,
            "projects": [],
            "bundles": [],
            "artifactErrors": [],
            "scannedAt": crate::time::now_iso()
        }));
    };

    let config = load_config(&root)?;
    let mut errors = Vec::new();
    let projects = read_artifacts(&root.join(".knit/projects"), ".project.json", &mut errors);
    let bundles = read_artifacts(&root.join(".knit/bundles"), ".bundle.json", &mut errors);
    if let Some(id) = bundle {
        let exists = bundles.iter().any(|value| value["id"].as_str() == Some(id));
        if !exists {
            bail!("bundle `{id}` does not exist in {}", root.display());
        }
    }
    Ok(json!({
        "apiVersion": API_VERSION,
        "workspaceRoot": root,
        "isKnit": true,
        "activeBundle": bundle.map(str::to_string).or(config.active_bundle),
        "activeProject": config.active_project,
        "projects": projects,
        "bundles": bundles,
        "artifactErrors": errors,
        "scannedAt": crate::time::now_iso()
    }))
}

fn read_artifacts(dir: &Path, suffix: &str, errors: &mut Vec<Value>) -> Vec<Value> {
    let mut paths = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.to_string_lossy().ends_with(suffix))
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            errors.push(json!({"file": dir, "error": error.to_string()}));
            return Vec::new();
        }
    };
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            match fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))
                .and_then(|text| serde_json::from_str(&text).context("invalid JSON"))
            {
                Ok(value) => Some(value),
                Err(error) => {
                    errors.push(json!({"file": path, "error": format!("{error:#}")}));
                    None
                }
            }
        })
        .collect()
}

fn watch(
    workspace: Option<&Path>,
    bundle: Option<&str>,
    interval_ms: u64,
    once: bool,
) -> Result<()> {
    let mut previous = None;
    loop {
        let value = snapshot(workspace, bundle)?;
        let comparable = comparable_snapshot(&value);
        if previous.as_ref() != Some(&comparable) {
            print_json(&json!({"kind": "snapshot", "snapshot": value}))?;
            previous = Some(comparable);
        }
        if once {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(interval_ms.max(50)));
    }
}

fn comparable_snapshot(value: &Value) -> String {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("scannedAt");
    }
    value.to_string()
}

fn read_input(source: &str) -> Result<Value> {
    let text = if source == "-" {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        text
    } else if source.trim_start().starts_with('{') {
        source.to_string()
    } else {
        fs::read_to_string(source).with_context(|| format!("failed to read {source}"))?
    };
    let value: Value = serde_json::from_str(&text).context("input must be a JSON object")?;
    if !value.is_object() {
        bail!("input must be a JSON object");
    }
    Ok(value)
}

#[derive(Clone)]
struct BuiltStep {
    argv: Vec<String>,
    tolerated_failure: Option<&'static str>,
}

impl BuiltStep {
    fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            tolerated_failure: None,
        }
    }
}

pub fn execute_action(
    action_id: &str,
    workspace: Option<&Path>,
    bundle: Option<&str>,
    session_id: Option<&str>,
    input: &Value,
) -> Result<Vec<Value>> {
    let action = ACTIONS
        .iter()
        .find(|action| action.id == action_id)
        .with_context(|| format!("unknown Knit action `{action_id}`"))?;
    if action.scope == "bundle" && bundle.is_none() {
        bail!("action `{action_id}` requires an explicit bundle");
    }
    validate_input(action, input)?;
    let steps = build_steps(action_id, bundle, input)?;
    let cwd = workspace
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir().context("failed to read current directory")?);
    let executable = std::env::current_exe().context("failed to locate knit executable")?;
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    );
    let mut events = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        events.push(json!({
            "kind": "started", "runId": run_id, "actionId": action_id,
            "step": index + 1, "argv": step.argv
        }));
        let mut command = Command::new(&executable);
        command
            .args(&step.argv)
            .current_dir(&cwd)
            .env_remove("KNIT_SESSION");
        if let Some(session_id) = session_id.filter(|session_id| !session_id.trim().is_empty()) {
            command.env("KNIT_SESSION", session_id);
        }
        let output = command
            .output()
            .with_context(|| format!("failed to execute action `{action_id}`"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !stdout.is_empty() {
            events.push(
                json!({"kind": "stdout", "runId": run_id, "actionId": action_id, "text": stdout}),
            );
        }
        if !stderr.is_empty() {
            events.push(
                json!({"kind": "stderr", "runId": run_id, "actionId": action_id, "text": stderr}),
            );
        }
        let code = output.status.code();
        if !output.status.success() {
            let combined = format!("{stdout}\n{stderr}");
            if step
                .tolerated_failure
                .is_some_and(|needle| combined.contains(needle))
            {
                events.push(json!({"kind": "progress", "runId": run_id, "actionId": action_id, "message": "tolerated step failure"}));
                continue;
            }
            events.push(json!({"kind": "error", "runId": run_id, "actionId": action_id, "exitCode": code, "message": "Knit command failed"}));
            return Ok(events);
        }
    }
    if !action.read_only {
        events.push(json!({"kind": "stateChanged", "runId": run_id, "actionId": action_id}));
    }
    events.push(json!({"kind": "result", "runId": run_id, "actionId": action_id, "success": true, "exitCode": 0}));
    Ok(events)
}

fn validate_input(action: &ActionDef, input: &Value) -> Result<()> {
    let object = input.as_object().context("input must be a JSON object")?;
    for param in action.params {
        let Some(value) = object.get(param.key) else {
            if param.required {
                bail!("action `{}` requires input `{}`", action.id, param.key);
            }
            continue;
        };
        let valid = match param.kind {
            "boolean" => value.is_boolean(),
            "array" => {
                value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string))
                    || value.is_string()
            }
            _ => value.is_string(),
        };
        if !valid {
            bail!(
                "action `{}` input `{}` has the wrong type",
                action.id,
                param.key
            );
        }
    }
    for key in object.keys() {
        if !action.params.iter().any(|param| param.key == key) {
            bail!("action `{}` does not accept input `{key}`", action.id);
        }
    }
    Ok(())
}

fn build_steps(action_id: &str, bundle: Option<&str>, input: &Value) -> Result<Vec<BuiltStep>> {
    let mut argv = Vec::new();
    let bundle_args = |argv: &mut Vec<String>| {
        if let Some(bundle) = bundle {
            argv.extend(["--bundle".to_string(), bundle.to_string()]);
        }
    };
    bundle_args(&mut argv);
    let s = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("").trim();
    let b = |key: &str| input.get(key).and_then(Value::as_bool).unwrap_or(false);
    let list = |key: &str| -> Vec<String> {
        match input.get(key) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            Some(Value::String(text)) => text.split_whitespace().map(str::to_string).collect(),
            _ => Vec::new(),
        }
    };
    let push_opt = |argv: &mut Vec<String>, flag: &str, value: &str| {
        if !value.is_empty() {
            argv.extend([flag.to_string(), value.to_string()]);
        }
    };
    match action_id {
        "commit" => argv.extend(["commit", "--all", "-m", s("message")].map(str::to_string)),
        "bundle.archive" => {
            argv.clear();
            argv.extend(["bundle", "archive", bundle.unwrap_or_default()].map(str::to_string));
            push_opt(&mut argv, "--reason", s("reason"));
            if b("keepWorktrees") {
                argv.push("--keep-worktrees".into());
            }
            if b("force") {
                argv.push("--force".into());
            }
        }
        "bundle.delete" => {
            argv.clear();
            argv.extend(
                [
                    "bundle",
                    "delete",
                    bundle.unwrap_or_default(),
                    "--force",
                    "--worktrees",
                ]
                .map(str::to_string),
            );
        }
        "bundle.validate" => argv.extend(["bundle", "validate"].map(str::to_string)),
        "bundle.add" => {
            argv.extend(["bundle", "add"].map(str::to_string));
            argv.extend(list("repos"));
        }
        "bundle.remove" => {
            argv.extend(["bundle", "remove"].map(str::to_string));
            argv.extend(list("repos"));
            if b("keepWorktree") {
                argv.push("--keep-worktree".into());
            }
            if b("force") {
                argv.push("--force".into());
            }
        }
        "bundle.worktree" => argv.extend(["bundle", "worktree"].map(str::to_string)),
        "bundle.pull" => {
            argv.clear();
            argv.extend(["bundle", "pull", s("slug"), "--json"].map(str::to_string));
        }
        "status" => argv.push("status".into()),
        "run.up" => argv.extend(["run", "up"].map(str::to_string)),
        "run.status" => argv.extend(["run", "status"].map(str::to_string)),
        "run.down" => argv.extend(["run", "down"].map(str::to_string)),
        "check.run" => argv.extend(["check", "run", s("name")].map(str::to_string)),
        "check.record" => {
            argv.extend(["check", "record", s("name")].map(str::to_string));
            argv.push(
                if s("verdict") == "fail" {
                    "--fail"
                } else {
                    "--pass"
                }
                .into(),
            );
            push_opt(&mut argv, "--detail", s("detail"));
        }
        "tag.create" => {
            argv.extend(["tag", s("name")].map(str::to_string));
            push_opt(&mut argv, "-m", s("message"));
        }
        "publish.create" => {
            argv.extend(["publish", "create"].map(str::to_string));
            push_opt(&mut argv, "--base", s("base"));
            if b("draft") {
                argv.push("--draft".into());
            }
        }
        "publish.sync" => argv.extend(["publish", "sync"].map(str::to_string)),
        "publish.status" => argv.extend(["publish", "status", "--live"].map(str::to_string)),
        "land.plan" => {
            argv.push("land".into());
            push_opt(&mut argv, "--target", s("target"));
            argv.push("plan".into());
        }
        "land.check" => argv.extend(["land", "check"].map(str::to_string)),
        "land.apply" | "land.ship" => {
            argv.push("land".into());
            push_opt(&mut argv, "--target", s("target"));
            argv.push("apply".into());
            if b("skipChecks") {
                argv.push("--skip-checks".into());
            }
        }
        "land.status" => argv.extend(["land", "status"].map(str::to_string)),
        "land.resume" => argv.extend(["land", "resume"].map(str::to_string)),
        "land.rollback" => {
            argv.extend(["land", "rollback"].map(str::to_string));
            if b("apply") {
                argv.push("--apply".into());
            }
        }
        "push" => {
            argv.push("push".into());
            if b("setUpstream") {
                argv.push("--set-upstream".into());
            }
        }
        "pull" => {
            argv.push("pull".into());
            if b("rebase") {
                argv.push("--rebase".into());
            }
            if b("main") {
                argv.push("--base".into());
            }
        }
        "fetch" => {
            argv.clear();
            argv.push("fetch".into());
        }
        "diff.stat" => argv.extend(["diff", "--stat"].map(str::to_string)),
        "merge" => {
            argv.extend(["merge", "--into", s("into")].map(str::to_string));
            if b("push") {
                argv.push("--push".into());
            }
        }
        "merge.continue" => argv.extend(["merge", "--continue"].map(str::to_string)),
        "merge.abort" => argv.extend(["merge", "--abort"].map(str::to_string)),
        "cherrypick.dryrun" | "cherrypick.apply" => {
            argv.clear();
            argv.extend(["cherrypick", "--from", s("from")].map(str::to_string));
            argv.extend(list("targets"));
            if action_id.ends_with("dryrun") {
                argv.push("--dry-run".into());
            }
        }
        "revert.plan" | "revert.apply" => {
            argv.extend(
                [
                    "revert",
                    s("target"),
                    if action_id.ends_with("plan") {
                        "--plan"
                    } else {
                        "--apply"
                    },
                ]
                .map(str::to_string),
            );
        }
        "bundle.create" => {
            argv.clear();
            argv.extend(["bundle", s("title")].map(str::to_string));
            push_opt(&mut argv, "--view", s("view"));
            for repo in list("repos") {
                argv.extend(["--repo".to_string(), repo]);
            }
        }
        "remote.add" => {
            argv.clear();
            argv.extend(["remote", "add", s("name"), s("url")].map(str::to_string));
            if b("global") {
                argv.push("--global".into());
            }
        }
        "remote.list" => {
            argv.clear();
            argv.extend(["remote", "list"].map(str::to_string));
            if b("global") {
                argv.push("--global".into());
            }
        }
        "remote.remove" => {
            argv.clear();
            argv.extend(["remote", "remove", s("name")].map(str::to_string));
            if b("global") {
                argv.push("--global".into());
            }
        }
        "remote.projects" => {
            argv.clear();
            argv.extend(["remote", "projects"].map(str::to_string));
            push_opt(&mut argv, "--remote", s("remote"));
            argv.push("--json".into());
        }
        "clone" => {
            argv.clear();
            argv.extend(["clone", s("project"), s("target"), "--json"].map(str::to_string));
        }
        "sync.push" => {
            argv.clear();
            argv.extend(["sync", "push", "--all"].map(str::to_string));
        }
        "sync.pull" => {
            argv.clear();
            argv.extend(["sync", "pull", "--all"].map(str::to_string));
        }
        "sync.pushKg" => {
            argv.clear();
            argv.extend(["sync", "push", "--kg"].map(str::to_string));
        }
        "history.list" => {
            argv.clear();
            argv.extend(
                [
                    "history",
                    "list",
                    "--limit",
                    if s("limit").is_empty() {
                        "20"
                    } else {
                        s("limit")
                    },
                ]
                .map(str::to_string),
            );
        }
        "prune.report" => {
            argv.clear();
            argv.extend(["bundle", "prune", "--report"].map(str::to_string));
        }
        "prune.apply" => {
            argv.clear();
            argv.extend(["bundle", "prune", "--apply"].map(str::to_string));
            if b("all") {
                argv.push("--all".into());
            } else {
                if b("worktrees") {
                    argv.push("--worktrees".into());
                }
                if b("branches") {
                    argv.push("--branches".into());
                }
            }
        }
        "doctor" => {
            argv.clear();
            argv.push("doctor".into());
        }
        "project.show" => {
            argv.clear();
            argv.extend(["project", "show"].map(str::to_string));
        }
        "view.list" => {
            argv.clear();
            argv.extend(["view", "list"].map(str::to_string));
        }
        _ => bail!("unknown Knit action `{action_id}`"),
    }

    if action_id == "push" {
        let mut commit = vec![
            "--bundle".to_string(),
            bundle.unwrap_or_default().to_string(),
        ];
        commit.extend(["commit", "--all", "-m", s("message")].map(str::to_string));
        return Ok(vec![
            BuiltStep {
                argv: commit,
                tolerated_failure: Some("No staged changes found"),
            },
            BuiltStep::new(argv),
        ]);
    }
    if action_id == "land.ship" {
        let mut plan = vec![
            "--bundle".to_string(),
            bundle.unwrap_or_default().to_string(),
            "land".to_string(),
        ];
        push_opt(&mut plan, "--target", s("target"));
        plan.push("plan".into());
        return Ok(vec![
            BuiltStep {
                argv: plan,
                tolerated_failure: Some("already exists"),
            },
            BuiltStep::new(argv),
        ]);
    }
    Ok(vec![BuiltStep::new(argv)])
}

fn serve_lines(handler: fn(&Value) -> Option<Value>) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_json_line(
                    &mut stdout,
                    &rpc_error(Value::Null, -32700, &error.to_string()),
                )?;
                continue;
            }
        };
        if let Some(response) = handler(&request) {
            write_json_line(&mut stdout, &response)?;
        }
    }
    Ok(())
}

fn handle_api_request(request: &Value) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str)?;
    let id = request.get("id").cloned()?;
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let known_method = matches!(
        method,
        "initialize"
            | "api.describe"
            | "action.list"
            | "api.snapshot"
            | "workspace.snapshot"
            | "api.run"
            | "action.run"
    );
    let result: Result<Value> = (|| match method {
        "initialize" => Ok(json!({
            "apiVersion": API_VERSION,
            "serverInfo": {"name": "knit", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"actions": true, "snapshots": true, "watch": false, "actionCancellation": false}
        })),
        "api.describe" => Ok(describe()),
        "action.list" => Ok(json!({
            "actions": ACTIONS.iter().map(action_descriptor).collect::<Vec<_>>()
        })),
        "api.snapshot" | "workspace.snapshot" => snapshot(
            params
                .get("workspace")
                .or_else(|| params.get("workspaceRoot"))
                .and_then(Value::as_str)
                .map(Path::new),
            params
                .get("bundle")
                .or_else(|| params.get("bundleId"))
                .and_then(Value::as_str),
        ),
        "api.run" | "action.run" => {
            let action_id = params
                .get("actionId")
                .and_then(Value::as_str)
                .context("missing actionId")?;
            let input = params.get("input").cloned().unwrap_or_else(|| json!({}));
            let events = execute_action(
                action_id,
                params
                    .get("workspace")
                    .or_else(|| params.get("workspaceRoot"))
                    .and_then(Value::as_str)
                    .map(Path::new),
                params
                    .get("bundle")
                    .or_else(|| params.get("bundleId"))
                    .and_then(Value::as_str),
                params.get("sessionId").and_then(Value::as_str),
                &input,
            )?;
            Ok(json!({"events": events}))
        }
        _ => bail!("method not found: {method}"),
    })();
    Some(match result {
        Ok(result) => rpc_result(id, result),
        Err(error) => rpc_error(
            id,
            if known_method { -32602 } else { -32601 },
            &format!("{error:#}"),
        ),
    })
}

fn handle_mcp_request(request: &Value) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str)?;
    let id = request.get("id").cloned()?;
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result: Result<Value> = (|| match method {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "knit", "version": env!("CARGO_PKG_VERSION")}
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": mcp_tools()})),
        "tools/call" => call_mcp_tool(&params),
        _ => bail!("method not found: {method}"),
    })();
    Some(match result {
        Ok(result) => rpc_result(id, result),
        Err(error) => rpc_error(
            id,
            if method == "tools/call" {
                -32602
            } else {
                -32601
            },
            &format!("{error:#}"),
        ),
    })
}

fn mcp_tools() -> Vec<Value> {
    let mut tools = vec![json!({
        "name": "knit_snapshot",
        "title": "Knit workspace snapshot",
        "description": "Read the current Knit workspace and bundle artifacts.",
        "inputSchema": {
            "type": "object", "additionalProperties": false,
            "properties": {
                "workspaceRoot": {"type": "string"},
                "bundleId": {"type": "string"}
            }
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
    })];
    tools.extend(ACTIONS.iter().map(|action| {
        let descriptor = action_descriptor(action);
        let mut schema = descriptor["inputSchema"].clone();
        let properties = schema["properties"].as_object_mut().expect("object schema");
        properties.insert(
            "workspaceRoot".into(),
            json!({"type": "string", "description": "Explicit Knit workspace root"}),
        );
        properties.insert(
            "bundleId".into(),
            json!({"type": "string", "description": "Explicit bundle id"}),
        );
        properties.insert(
            "sessionId".into(),
            json!({"type": "string", "description": "Session identity recorded on ledger nodes"}),
        );
        if action.scope == "bundle" {
            schema["required"]
                .as_array_mut()
                .expect("required array")
                .push(json!("bundleId"));
        }
        json!({
            "name": mcp_name(action.id),
            "title": action.title,
            "description": action.description,
            "inputSchema": schema,
            "annotations": {
                "readOnlyHint": action.read_only,
                "destructiveHint": action.destructive,
                "idempotentHint": action.read_only,
                "openWorldHint": action.open_world
            }
        })
    }));
    tools
}

fn mcp_name(action_id: &str) -> String {
    format!("knit_{}", action_id.replace('.', "_"))
}

fn call_mcp_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("missing tool name")?;
    let mut arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if name == "knit_snapshot" {
        let value = snapshot(
            arguments
                .get("workspaceRoot")
                .and_then(Value::as_str)
                .map(Path::new),
            arguments.get("bundleId").and_then(Value::as_str),
        )?;
        return Ok(mcp_content(&value, false));
    }
    let action = ACTIONS
        .iter()
        .find(|action| mcp_name(action.id) == name)
        .context("unknown Knit tool")?;
    let workspace = arguments
        .get("workspaceRoot")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let bundle = arguments
        .get("bundleId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let session_id = arguments
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(object) = arguments.as_object_mut() {
        object.remove("workspaceRoot");
        object.remove("bundleId");
        object.remove("sessionId");
    }
    let events = execute_action(
        action.id,
        workspace.as_deref(),
        bundle.as_deref(),
        session_id.as_deref(),
        &arguments,
    )?;
    let failed = events.iter().any(|event| event["kind"] == "error");
    Ok(mcp_content(&json!({"events": events}), failed))
}

fn mcp_content(value: &Value, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())}],
        "structuredContent": value,
        "isError": is_error
    })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn print_json(value: &Value) -> Result<()> {
    let stdout = io::stdout();
    write_json_line(&mut stdout.lock(), value)
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn preserves_original_action_ids() {
        assert_eq!(ACTIONS.len(), 51);
        assert!(ACTIONS.iter().any(|action| action.id == "land.ship"));
        assert!(ACTIONS.iter().any(|action| action.id == "sync.pushKg"));
    }

    #[test]
    fn bundle_actions_always_pin_the_bundle() {
        let steps = build_steps("diff.stat", Some("feature-a"), &json!({})).unwrap();
        assert_eq!(steps[0].argv, ["--bundle", "feature-a", "diff", "--stat"]);
    }

    #[test]
    fn mcp_catalog_derives_safety_annotations() {
        let tools = mcp_tools();
        let delete = tools
            .iter()
            .find(|tool| tool["name"] == "knit_bundle_delete")
            .unwrap();
        assert_eq!(delete["annotations"]["destructiveHint"], true);
        assert_eq!(delete["inputSchema"]["required"], json!(["bundleId"]));
    }

    #[test]
    fn every_catalog_action_builds_argv_accepted_by_clap() {
        for action in ACTIONS {
            let mut input = Map::new();
            for param in action.params.iter().filter(|param| param.required) {
                let value = match (action.id, param.key, param.kind) {
                    ("check.record", "verdict", _) => json!("pass"),
                    (_, _, "array") => json!(["sample"]),
                    (_, _, "boolean") => json!(true),
                    (_, "limit", _) => json!("1"),
                    _ => json!("sample"),
                };
                input.insert(param.key.to_string(), value);
            }
            let steps = build_steps(action.id, Some("sample-bundle"), &Value::Object(input))
                .unwrap_or_else(|error| panic!("{} did not build: {error:#}", action.id));
            for step in steps {
                let args = std::iter::once("knit".to_string())
                    .chain(step.argv)
                    .collect::<Vec<_>>();
                crate::cli::Cli::try_parse_from(args)
                    .unwrap_or_else(|error| panic!("{} built invalid argv: {error}", action.id));
            }
        }
    }
}
