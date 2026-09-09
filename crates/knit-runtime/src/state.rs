//! Runtime run state and the `knit run down|status` verbs. State is recorded
//! under `.knit/runtime-runs/<bundle>/` after a successful start; containers
//! are resolved by compose project label so down/status survive missing state
//! and torn-down worktrees.

use crate::config::{DatabaseMode, RuntimeMode};
use crate::support::{out, read_json};
use crate::transform::ServicePort;
use crate::RuntimeContext;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn frontend_port(ports: &[ServicePort]) -> Option<u16> {
    ports
        .iter()
        .find(|port| port.service == "frontend")
        .or_else(|| ports.iter().find(|port| port.service.contains("front")))
        .or_else(|| ports.first())
        .map(|port| port.host)
}

pub(crate) fn run_down(ctx: &RuntimeContext, purge: bool) -> Result<()> {
    // Project-scoped `down` resolves containers by compose label, so it works
    // even after the bundle worktree (and its compose file) is gone. With
    // recorded state, tear down exactly the stacks it lists; without state
    // (an `up` that failed before recording), sweep the legacy single-stack
    // name plus the per-repo names multi-stack runs derive.
    let mut targets: Vec<(String, Vec<String>)> = Vec::new();
    match load_runtime_state(ctx).ok() {
        Some(state) if !state.stacks.is_empty() => {
            for stack in &state.stacks {
                targets.push((stack.project_name.clone(), stack.profiles.clone()));
            }
        }
        Some(state) => {
            targets.push((compose_project_name(&ctx.bundle_id), state.profiles));
        }
        None => {
            let legacy = compose_project_name(&ctx.bundle_id);
            for repo in &ctx.repos {
                targets.push((format!("{legacy}--{}", repo.id), Vec::new()));
            }
            targets.push((legacy, Vec::new()));
        }
    }

    for (project_name, profiles) in &targets {
        // Named volumes are restart data, but anonymous volumes cannot be
        // reattached by a later `up`. Remove service containers with `rm -v`
        // before ordinary `down` so those anonymous volumes do not become
        // unowned hashes that no later project-scoped purge can identify.
        if !purge {
            let mut remove = Command::new("docker");
            remove.args(["compose", "-p", project_name]);
            for profile in profiles {
                remove.args(["--profile", profile]);
            }
            let status = remove
                .args(["rm", "--force", "--stop", "--volumes"])
                .status()
                .context("failed to remove docker compose containers")?;
            if !status.success() {
                bail!("docker compose rm exited with status {status}");
            }
        }

        let mut command = Command::new("docker");
        command.args(["compose", "-p", project_name]);
        for profile in profiles {
            command.args(["--profile", profile]);
        }
        command.args(["down", "--remove-orphans"]);
        if purge {
            command.args(["--volumes", "--rmi", "local"]);
        }
        let status = command
            .status()
            .context("failed to run docker compose down")?;

        if !status.success() {
            bail!("docker compose down exited with status {status}");
        }
    }

    println!(
        "{} {}",
        out::heading(if purge {
            "Runtime purged:"
        } else {
            "Runtime down:"
        }),
        out::repo(&ctx.bundle_id)
    );

    // The generated compose files carry resolved app configuration; a
    // finished run has no reason to keep them around.
    let run_dir = runtime_run_dir(&ctx.root, &ctx.bundle_id);
    if run_dir.exists() {
        match std::fs::remove_dir_all(&run_dir) {
            Ok(()) => println!(
                "{} {}",
                out::muted("Removed:"),
                out::path(run_dir.display())
            ),
            Err(error) => println!(
                "{} could not remove {}: {error}",
                out::warn("Warn:"),
                run_dir.display()
            ),
        }
    }
    Ok(())
}

pub(crate) fn run_status(ctx: &RuntimeContext) -> Result<()> {
    // State may be missing when an `up` failed before recording it; still
    // report containers resolved by compose label so cleanup is visible.
    let state = load_runtime_state(ctx).ok();

    // (stack label, compose project) pairs to report; single-stack runs keep
    // the unlabelled legacy shape.
    let views: Vec<(Option<String>, String)> = match &state {
        Some(state) if !state.stacks.is_empty() => state
            .stacks
            .iter()
            .map(|stack| (Some(stack.repo.clone()), stack.project_name.clone()))
            .collect(),
        _ => vec![(None, compose_project_name(&ctx.bundle_id))],
    };

    println!("{} {}", out::heading("Bundle:"), out::repo(&ctx.bundle_id));
    let mut any_running = false;
    let mut services: Vec<(String, String)> = Vec::new();
    for (label, project_name) in &views {
        let stack_services = compose_service_states(project_name);
        any_running |= stack_services.iter().any(|(_, state)| state == "running");
        if let Some(label) = label {
            println!("{} {}", out::heading("Stack:"), out::repo(label));
        }
        if stack_services.is_empty() {
            println!(
                "{} {}",
                out::heading("Services:"),
                out::muted("none running")
            );
        } else {
            for (service, service_state) in &stack_services {
                println!("{} {}", out::heading(format!("{service}:")), service_state);
            }
        }
        services.extend(stack_services);
    }

    let Some(state) = state else {
        println!(
            "{} No recorded runtime state. Run `knit run up`{}.",
            out::heading("Next:"),
            if services.is_empty() {
                ""
            } else {
                ", or `knit run down` to clean up the containers above"
            }
        );
        return Ok(());
    };

    for port in &state.ports {
        match port.source_host.filter(|source| *source != port.host) {
            Some(source) => println!(
                "{} {} localhost:{} {}",
                out::muted("Port:"),
                port.service,
                port.host,
                out::muted(format!("(source {source})"))
            ),
            None => println!(
                "{} {} localhost:{}",
                out::muted("Port:"),
                port.service,
                port.host
            ),
        }
    }
    if let Some(database) = &state.database {
        println!(
            "{} {} localhost:{} ({})",
            out::heading("Database:"),
            database_status_label(database, &services),
            database.port,
            database.name
        );
    }
    if let (Some(profile), Some(frontend)) = (&state.profile_path, frontend_port(&state.ports)) {
        if any_running {
            println!(
                "{} http://localhost:{}{}",
                out::heading("Profile:"),
                frontend,
                profile
            );
        } else {
            println!(
                "{} http://localhost:{}{} {}",
                out::heading("Profile:"),
                frontend,
                profile,
                out::muted("(stack stopped)")
            );
        }
    }
    if state.stacks.len() > 1 {
        for stack in &state.stacks {
            println!(
                "{} {} {}",
                out::muted("Compose:"),
                out::repo(&stack.repo),
                stack.compose_file
            );
        }
    } else {
        println!("{} {}", out::muted("Compose:"), state.compose_file);
    }
    if !any_running {
        println!(
            "{} Runtime is stopped. Run `knit run up` from a stack worktree checkout.",
            out::heading("Next:")
        );
    }
    Ok(())
}

/// `knit run status --json`: the same facts as the human report, as one JSON
/// object on stdout and nothing else.
pub(crate) fn run_status_json(ctx: &RuntimeContext) -> Result<()> {
    let state = load_runtime_state(ctx).ok();
    let projects: Vec<String> = match &state {
        Some(state) => recorded_stacks(&ctx.bundle_id, state)
            .into_iter()
            .map(|stack| stack.project_name)
            .collect(),
        // No state (an `up` that failed before recording, or a run started by
        // an older knit): report whatever the derived project names still own.
        None => derived_project_names(ctx),
    };
    let collected: Vec<(String, Vec<(String, String)>)> = projects
        .into_iter()
        .map(|project_name| {
            let services = compose_service_states(&project_name);
            (project_name, services)
        })
        .collect();

    let report = build_status_json(&ctx.bundle_id, state.as_ref(), &collected);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// The compose projects a bundle's run could be using without recorded state:
/// the legacy single-stack name plus the per-repo names multi-stack runs
/// derive. Mirrors what `run_down` sweeps.
fn derived_project_names(ctx: &RuntimeContext) -> Vec<String> {
    let legacy = compose_project_name(&ctx.bundle_id);
    let mut names = vec![legacy.clone()];
    names.extend(
        ctx.repos
            .iter()
            .map(|repo| format!("{legacy}--{}", repo.id)),
    );
    names
}

/// The stacks a run state describes. State written before multi-stack runs
/// existed only has the top-level fields, which describe one stack under the
/// legacy project name.
fn recorded_stacks(bundle_id: &str, state: &RuntimeRunState) -> Vec<RuntimeStackState> {
    if !state.stacks.is_empty() {
        return state.stacks.clone();
    }
    vec![RuntimeStackState {
        repo: state.stack_repo.clone(),
        project_name: compose_project_name(bundle_id),
        mode: state.mode,
        compose_file: state.compose_file.clone(),
        override_file: None,
        ports: state.ports.clone(),
        profiles: state.profiles.clone(),
        env: state.env.clone(),
        database: state.database.clone(),
    }]
}

/// Assemble the JSON report from the run state and the `(project name,
/// services)` pairs already collected from docker, so the serialization is
/// testable on its own.
fn build_status_json(
    bundle_id: &str,
    state: Option<&RuntimeRunState>,
    collected: &[(String, Vec<(String, String)>)],
) -> StatusReport {
    let services_of = |project_name: &str| -> Vec<StatusService> {
        collected
            .iter()
            .find(|(name, _)| name == project_name)
            .map(|(_, services)| {
                services
                    .iter()
                    .map(|(name, state)| StatusService {
                        name: name.clone(),
                        state: state.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let stacks: Vec<StatusStack> = match state {
        Some(state) => recorded_stacks(bundle_id, state)
            .into_iter()
            .map(|stack| StatusStack {
                services: services_of(&stack.project_name),
                repo: Some(stack.repo),
                project_name: stack.project_name,
                mode: Some(stack.mode),
                compose_file: Some(stack.compose_file),
                override_file: stack.override_file,
                ports: stack.ports,
                database: stack.database,
            })
            .collect(),
        // Without state a project is only worth reporting when it still owns
        // containers; the derived names are guesses.
        None => collected
            .iter()
            .filter(|(_, services)| !services.is_empty())
            .map(|(project_name, _)| StatusStack {
                repo: None,
                project_name: project_name.clone(),
                mode: None,
                compose_file: None,
                override_file: None,
                services: services_of(project_name),
                ports: Vec::new(),
                database: None,
            })
            .collect(),
    };

    StatusReport {
        bundle_id: bundle_id.to_string(),
        recorded: state.is_some(),
        running: stacks.iter().any(|stack| {
            stack
                .services
                .iter()
                .any(|service| service.state == "running")
        }),
        profile_path: state.and_then(|state| state.profile_path.clone()),
        frontend_port: state.and_then(|state| frontend_port(&state.ports)),
        started_at: state.map(|state| state.started_at.clone()),
        stacks,
    }
}

/// `knit run status --json` output. Keys are camelCase and match
/// `state.json`; optional facts are omitted rather than emitted as null, so a
/// consumer can tell "absent" from "recorded as empty". `repo` is the one
/// exception: it is explicitly null for a stack resolved by project name
/// alone.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusReport {
    bundle_id: String,
    recorded: bool,
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frontend_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    stacks: Vec<StatusStack>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusStack {
    repo: Option<String>,
    project_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<RuntimeMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compose_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    override_file: Option<String>,
    services: Vec<StatusService>,
    ports: Vec<ServicePort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    database: Option<StateDatabase>,
}

#[derive(Debug, Serialize)]
struct StatusService {
    name: String,
    state: String,
}

/// The isolated Compose project name a bundle's runtime runs under, so two
/// bundles can bring the same stack up side by side.
pub(crate) fn compose_project_name(bundle_id: &str) -> String {
    format!("knit-run-{bundle_id}")
}

pub(crate) fn has_state(ctx: &RuntimeContext) -> bool {
    runtime_run_dir(&ctx.root, &ctx.bundle_id)
        .join("state.json")
        .exists()
}

fn load_runtime_state(ctx: &RuntimeContext) -> Result<RuntimeRunState> {
    let state_path = runtime_run_dir(&ctx.root, &ctx.bundle_id).join("state.json");
    if !state_path.exists() {
        bail!(
            "No runtime state found for bundle `{}`. Run `knit run up` first.",
            ctx.bundle_id
        );
    }
    read_json(&state_path)
}

/// `(service, state)` pairs from `docker compose ps` for a compose project,
/// resolved by label so no compose file is needed. Empty when docker is
/// unavailable or nothing is running.
fn compose_service_states(project_name: &str) -> Vec<(String, String)> {
    let Ok(output) = Command::new("docker")
        .args(["compose", "-p", project_name, "ps", "--format", "json"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_compose_ps(&text)
}

/// Parse `docker compose ps --format json` output, which is a JSON array in
/// some compose versions and newline-delimited objects in others.
fn parse_compose_ps(text: &str) -> Vec<(String, String)> {
    let entries: Vec<serde_json::Value> =
        if let Ok(serde_json::Value::Array(values)) = serde_json::from_str(text.trim()) {
            values
        } else {
            text.lines()
                .filter_map(|line| serde_json::from_str(line.trim()).ok())
                .collect()
        };

    entries
        .iter()
        .filter_map(|entry| {
            let service = entry.get("Service")?.as_str()?.to_string();
            let state = entry.get("State")?.as_str()?.to_string();
            Some((service, state))
        })
        .collect()
}

/// How to describe the database in `knit run status`: a bundle-owned database
/// reports its container's state, a shared one is only ever pointed at.
fn database_status_label(database: &StateDatabase, services: &[(String, String)]) -> &'static str {
    if database.mode == DatabaseMode::Bundle {
        let running = services
            .iter()
            .any(|(service, service_state)| service == "db" && service_state == "running");
        if running {
            "running"
        } else {
            "stopped"
        }
    } else if TcpStream::connect(format!("127.0.0.1:{}", database.port)).is_ok() {
        "reachable"
    } else {
        "unreachable"
    }
}

pub(crate) fn runtime_run_dir(root: &Path, bundle_id: &str) -> PathBuf {
    root.join(".knit/runtime-runs").join(bundle_id)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeRunState {
    pub(crate) bundle_id: String,
    pub(crate) stack_repo: String,
    #[serde(default)]
    pub(crate) mode: RuntimeMode,
    #[serde(default)]
    pub(crate) ports: Vec<ServicePort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) database: Option<StateDatabase>,
    /// Workspace-relative path of the compose file this run executed
    /// (generated file in transform mode, repo file in contract mode).
    pub(crate) compose_file: String,
    /// Compose profiles activated by this run (e.g. `bundle-db`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) profiles: Vec<String>,
    /// The injected environment contract (contract mode), recorded so the
    /// same compose file can be driven manually for debugging.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) profile_path: Option<String>,
    pub(crate) started_at: String,
    /// Every stack this run started. Single-stack runs also mirror the first
    /// stack into the legacy top-level fields above so older tooling keeps
    /// reading state files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) stacks: Vec<RuntimeStackState>,
}

/// One stack of a runtime run: a bundle repo's compose lifted into its own
/// per-bundle compose project.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeStackState {
    pub(crate) repo: String,
    pub(crate) project_name: String,
    #[serde(default)]
    pub(crate) mode: RuntimeMode,
    pub(crate) compose_file: String,
    /// Workspace-relative path of the engine override compose file this stack
    /// ran with, when the runtime had an [`crate::EngineView`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) override_file: Option<String>,
    #[serde(default)]
    pub(crate) ports: Vec<ServicePort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) database: Option<StateDatabase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StateDatabase {
    #[serde(default)]
    pub(crate) mode: DatabaseMode,
    pub(crate) port: u16,
    pub(crate) name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_compose_ps_accepts_array_and_ndjson() {
        let array =
            r#"[{"Service":"backend","State":"running"},{"Service":"db","State":"exited"}]"#;
        assert_eq!(
            parse_compose_ps(array),
            vec![
                ("backend".to_string(), "running".to_string()),
                ("db".to_string(), "exited".to_string())
            ]
        );
        let ndjson = "{\"Service\":\"frontend\",\"State\":\"running\"}\n{\"Service\":\"backend\",\"State\":\"running\"}\n";
        assert_eq!(
            parse_compose_ps(ndjson),
            vec![
                ("frontend".to_string(), "running".to_string()),
                ("backend".to_string(), "running".to_string())
            ]
        );
    }

    fn recorded_state() -> RuntimeRunState {
        let ports = vec![ServicePort {
            service: "backend".into(),
            host: 4011,
            container: Some(4000),
            source_host: Some(4001),
        }];
        RuntimeRunState {
            bundle_id: "my-bundle".into(),
            stack_repo: "knithub".into(),
            mode: RuntimeMode::Contract,
            ports: ports.clone(),
            database: None,
            compose_file: ".knit/runtime-runs/my-bundle/docker-compose.yml".into(),
            profiles: Vec::new(),
            env: BTreeMap::new(),
            profile_path: Some("/app/profile".into()),
            started_at: "2026-09-09T10:00:00Z".into(),
            stacks: vec![RuntimeStackState {
                repo: "knithub".into(),
                project_name: "knit-run-my-bundle".into(),
                mode: RuntimeMode::Contract,
                compose_file: ".knit/runtime-runs/my-bundle/docker-compose.yml".into(),
                override_file: Some(
                    ".knit/runtime-runs/my-bundle/docker-compose.engine.yml".into(),
                ),
                ports,
                profiles: Vec::new(),
                env: BTreeMap::new(),
                database: Some(StateDatabase {
                    mode: DatabaseMode::Shared,
                    port: 5436,
                    name: "knithub_dev".into(),
                }),
            }],
        }
    }

    #[test]
    fn status_json_reports_the_documented_shape_for_a_recorded_run() {
        let state = recorded_state();
        let collected = vec![(
            "knit-run-my-bundle".to_string(),
            vec![("backend".to_string(), "running".to_string())],
        )];

        let report = build_status_json("my-bundle", Some(&state), &collected);
        let value = serde_json::to_value(&report).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "bundleId": "my-bundle",
                "recorded": true,
                "running": true,
                "profilePath": "/app/profile",
                "frontendPort": 4011,
                "startedAt": "2026-09-09T10:00:00Z",
                "stacks": [{
                    "repo": "knithub",
                    "projectName": "knit-run-my-bundle",
                    "mode": "contract",
                    "composeFile": ".knit/runtime-runs/my-bundle/docker-compose.yml",
                    "overrideFile": ".knit/runtime-runs/my-bundle/docker-compose.engine.yml",
                    "services": [{ "name": "backend", "state": "running" }],
                    "ports": [{
                        "service": "backend",
                        "host": 4011,
                        "container": 4000,
                        "sourceHost": 4001
                    }],
                    "database": { "mode": "shared", "port": 5436, "name": "knithub_dev" }
                }]
            })
        );
    }

    #[test]
    fn status_json_lists_only_projects_with_containers_when_no_state_was_recorded() {
        let collected = vec![
            ("knit-run-my-bundle".to_string(), Vec::new()),
            (
                "knit-run-my-bundle--knithub".to_string(),
                vec![("backend".to_string(), "exited".to_string())],
            ),
        ];

        let report = build_status_json("my-bundle", None, &collected);
        let value = serde_json::to_value(&report).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "bundleId": "my-bundle",
                "recorded": false,
                "running": false,
                "stacks": [{
                    "repo": null,
                    "projectName": "knit-run-my-bundle--knithub",
                    "services": [{ "name": "backend", "state": "exited" }],
                    "ports": []
                }]
            })
        );
    }

    #[test]
    fn frontend_port_prefers_frontend_service() {
        let ports = vec![
            ServicePort {
                service: "db".into(),
                host: 5446,
                container: Some(5432),
                source_host: None,
            },
            ServicePort {
                service: "web-frontend".into(),
                host: 5184,
                container: Some(5173),
                source_host: None,
            },
        ];
        assert_eq!(frontend_port(&ports), Some(5184));
        assert_eq!(frontend_port(&[]), None);
    }
}
