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

pub(crate) fn run_down(ctx: &RuntimeContext) -> Result<()> {
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
        let mut command = Command::new("docker");
        command.args(["compose", "-p", project_name]);
        for profile in profiles {
            command.args(["--profile", profile]);
        }
        let status = command
            .args(["down", "--remove-orphans"])
            .status()
            .context("failed to run docker compose down")?;

        if !status.success() {
            bail!("docker compose down exited with status {status}");
        }
    }

    println!(
        "{} {}",
        out::heading("Runtime down:"),
        out::repo(&ctx.bundle_id)
    );
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
        println!(
            "{} {} localhost:{}",
            out::muted("Port:"),
            port.service,
            port.host
        );
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

/// Build the `KNIT_*` environment contract for a runtime: bundle identity,
/// per-repo checkout paths and revisions, allocated ports, and the resolved
/// database. Covers every project repo plus any ad-hoc bundle repos; a repo
/// tracked in the bundle resolves to its bundle checkout, anything else to

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

/// Allocate one free host port per contract-mode service pool, stepping all

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

    #[test]
    fn frontend_port_prefers_frontend_service() {
        let ports = vec![
            ServicePort {
                service: "db".into(),
                host: 5446,
                container: Some(5432),
            },
            ServicePort {
                service: "web-frontend".into(),
                host: 5184,
                container: Some(5173),
            },
        ];
        assert_eq!(frontend_port(&ports), Some(5184));
        assert_eq!(frontend_port(&[]), None);
    }
}
